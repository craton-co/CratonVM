// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![allow(non_camel_case_types, non_snake_case, dead_code)]

//! OpenCL backend via dynamic runtime loading (`libloading`).
//!
//! Provides portable GPU/accelerator offload across NVIDIA, AMD, Intel, and Apple platforms
//! supporting OpenCL 1.2, 2.0, and 3.0.
//!
//! Zero build-time dependencies on OpenCL SDK or libclang: `OpenCL.dll` (Windows),
//! `libOpenCL.so.1`/`libOpenCL.so` (Linux), and `OpenCL.framework` (macOS) are resolved
//! at runtime. If the runtime library is not present on the host, all entry points return
//! [`DeviceError::NoDriver`] cleanly without panicking.

pub(crate) use crate::allocator::device_pool_enabled;
use crate::allocator::{
    AllocLease, BackingSlab, RetireState, SlabBackend, SubAllocator, DEFAULT_SLAB_BYTES,
    MIN_BLOCK_ALIGN,
};
use crate::{
    DeviceArch, DeviceCaps, DeviceError, KernelArgs, LaunchConfig, OpenClCaps, Result,
    SharedMemoryCaps,
};
use libloading::{Library, Symbol};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_void, CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// ── OpenCL 1.2+ Standard Types and Constants ─────────────────────────

pub(crate) type cl_int = i32;
pub(crate) type cl_uint = u32;
pub(crate) type cl_ulong = u64;
pub(crate) type cl_bool = cl_uint;
pub(crate) type cl_bitfield = cl_ulong;
pub(crate) type cl_device_type = cl_bitfield;
pub(crate) type cl_platform_info = cl_uint;
pub(crate) type cl_device_info = cl_uint;
pub(crate) type cl_mem_flags = cl_bitfield;
pub(crate) type cl_event_info = cl_uint;
pub(crate) type cl_program_build_info = cl_uint;
pub(crate) type cl_kernel_work_group_info = cl_uint;
pub(crate) type cl_map_flags = cl_bitfield;
pub(crate) type cl_command_queue_properties = cl_bitfield;

pub(crate) type cl_platform_id = *mut c_void;
pub(crate) type cl_device_id = *mut c_void;
pub(crate) type cl_context = *mut c_void;
pub(crate) type cl_command_queue = *mut c_void;
pub(crate) type cl_mem = *mut c_void;
pub(crate) type cl_program = *mut c_void;
pub(crate) type cl_kernel = *mut c_void;
pub(crate) type cl_event = *mut c_void;

pub(crate) const CL_SUCCESS: cl_int = 0;
pub(crate) const CL_DEVICE_NOT_FOUND: cl_int = -1;
pub(crate) const CL_DEVICE_NOT_AVAILABLE: cl_int = -2;
pub(crate) const CL_BUILD_PROGRAM_FAILURE: cl_int = -11;

pub(crate) const CL_DEVICE_TYPE_GPU: cl_device_type = 1 << 2;
pub(crate) const CL_DEVICE_TYPE_ACCELERATOR: cl_device_type = 1 << 3;
pub(crate) const CL_DEVICE_TYPE_DEFAULT: cl_device_type = 1 << 0;
pub(crate) const CL_DEVICE_TYPE_ALL: cl_device_type = 0xFFFFFFFF;

pub(crate) const CL_DEVICE_NAME: cl_device_info = 0x102B;
pub(crate) const CL_DEVICE_VENDOR: cl_device_info = 0x102C;
pub(crate) const CL_DRIVER_VERSION: cl_device_info = 0x102D;
pub(crate) const CL_DEVICE_VERSION: cl_device_info = 0x102F;
pub(crate) const CL_DEVICE_EXTENSIONS: cl_device_info = 0x1030;
pub(crate) const CL_DEVICE_MAX_COMPUTE_UNITS: cl_device_info = 0x1002;
pub(crate) const CL_DEVICE_MAX_WORK_GROUP_SIZE: cl_device_info = 0x1004;
pub(crate) const CL_DEVICE_MEM_BASE_ADDR_ALIGN: cl_device_info = 0x1019;
pub(crate) const CL_DEVICE_GLOBAL_MEM_SIZE: cl_device_info = 0x101F;
pub(crate) const CL_DEVICE_LOCAL_MEM_SIZE: cl_device_info = 0x1023;
pub(crate) const CL_DEVICE_HOST_UNIFIED_MEMORY: cl_device_info = 0x1035;
pub(crate) const CL_DEVICE_SVM_CAPABILITIES: cl_device_info = 0x1053;
// AMD cl_amd_device_attribute_query extensions (wavefront width & GFXIP major).
pub(crate) const CL_DEVICE_WAVEFRONT_WIDTH_AMD: cl_device_info = 0x4043;
pub(crate) const CL_DEVICE_GFXIP_MAJOR_AMD: cl_device_info = 0x404A;
pub(crate) const CL_BUFFER_CREATE_TYPE_REGION: cl_uint = 0x1220;

#[repr(C)]
#[derive(Clone, Copy)]
struct ClBufferRegion {
    origin: usize,
    size: usize,
}

pub(crate) const CL_PLATFORM_NAME: cl_platform_info = 0x0902;
pub(crate) const CL_PLATFORM_VERSION: cl_platform_info = 0x0901;

pub(crate) const CL_PROGRAM_BUILD_LOG: cl_program_build_info = 0x1183;
pub(crate) const CL_KERNEL_WORK_GROUP_SIZE: cl_kernel_work_group_info = 0x11B0;
pub(crate) const CL_KERNEL_PREFERRED_WORK_GROUP_SIZE_MULTIPLE: cl_kernel_work_group_info = 0x11B3;

pub(crate) const CL_EVENT_COMMAND_EXECUTION_STATUS: cl_event_info = 0x11D3;
pub(crate) const CL_COMPLETE: cl_int = 0x0;
pub(crate) const CL_RUNNING: cl_int = 0x1;
pub(crate) const CL_SUBMITTED: cl_int = 0x2;
pub(crate) const CL_QUEUED: cl_int = 0x3;

pub(crate) const CL_MEM_READ_WRITE: cl_mem_flags = 1 << 0;
pub(crate) const CL_MEM_USE_HOST_PTR: cl_mem_flags = 1 << 3;
pub(crate) const CL_MEM_ALLOC_HOST_PTR: cl_mem_flags = 1 << 4;
pub(crate) const CL_MEM_COPY_HOST_PTR: cl_mem_flags = 1 << 5;

pub(crate) const CL_MAP_READ: cl_map_flags = 1 << 0;
pub(crate) const CL_MAP_WRITE: cl_map_flags = 1 << 1;

pub(crate) const CL_TRUE: cl_bool = 1;
pub(crate) const CL_FALSE: cl_bool = 0;

pub(crate) type ClEventCallback = unsafe extern "C" fn(
    event: cl_event,
    event_command_exec_status: cl_int,
    user_data: *mut c_void,
);

// ── Dynamic Function Table ────────────────────────────────────────────

#[allow(non_snake_case)]
struct OpenClFunctions {
    _lib: Library,
    clGetPlatformIDs: unsafe extern "C" fn(
        num_entries: cl_uint,
        platforms: *mut cl_platform_id,
        num_platforms: *mut cl_uint,
    ) -> cl_int,
    clGetPlatformInfo: unsafe extern "C" fn(
        platform: cl_platform_id,
        param_name: cl_platform_info,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> cl_int,
    clGetDeviceIDs: unsafe extern "C" fn(
        platform: cl_platform_id,
        device_type: cl_device_type,
        num_entries: cl_uint,
        devices: *mut cl_device_id,
        num_devices: *mut cl_uint,
    ) -> cl_int,
    clGetDeviceInfo: unsafe extern "C" fn(
        device: cl_device_id,
        param_name: cl_device_info,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> cl_int,
    clCreateContext: unsafe extern "C" fn(
        properties: *const isize,
        num_devices: cl_uint,
        devices: *const cl_device_id,
        pfn_notify: Option<unsafe extern "C" fn(*const std::os::raw::c_char, *const c_void, usize, *mut c_void)>,
        user_data: *mut c_void,
        errcode_ret: *mut cl_int,
    ) -> cl_context,
    clReleaseContext: unsafe extern "C" fn(context: cl_context) -> cl_int,
    clCreateCommandQueue: unsafe extern "C" fn(
        context: cl_context,
        device: cl_device_id,
        properties: cl_command_queue_properties,
        errcode_ret: *mut cl_int,
    ) -> cl_command_queue,
    clReleaseCommandQueue: unsafe extern "C" fn(command_queue: cl_command_queue) -> cl_int,
    clCreateBuffer: unsafe extern "C" fn(
        context: cl_context,
        flags: cl_mem_flags,
        size: usize,
        host_ptr: *mut c_void,
        errcode_ret: *mut cl_int,
    ) -> cl_mem,
    clCreateSubBuffer: Option<
        unsafe extern "C" fn(
            buffer: cl_mem,
            flags: cl_mem_flags,
            buffer_create_type: cl_uint,
            buffer_create_info: *const c_void,
            errcode_ret: *mut cl_int,
        ) -> cl_mem,
    >,
    clRetainMemObject: Option<unsafe extern "C" fn(memobj: cl_mem) -> cl_int>,
    clReleaseMemObject: unsafe extern "C" fn(memobj: cl_mem) -> cl_int,
    clEnqueueReadBuffer: unsafe extern "C" fn(
        command_queue: cl_command_queue,
        buffer: cl_mem,
        blocking_read: cl_bool,
        offset: usize,
        size: usize,
        ptr: *mut c_void,
        num_events_in_wait_list: cl_uint,
        event_wait_list: *const cl_event,
        event: *mut cl_event,
    ) -> cl_int,
    clEnqueueWriteBuffer: unsafe extern "C" fn(
        command_queue: cl_command_queue,
        buffer: cl_mem,
        blocking_write: cl_bool,
        offset: usize,
        size: usize,
        ptr: *const c_void,
        num_events_in_wait_list: cl_uint,
        event_wait_list: *const cl_event,
        event: *mut cl_event,
    ) -> cl_int,
    clEnqueueFillBuffer: Option<
        unsafe extern "C" fn(
            command_queue: cl_command_queue,
            buffer: cl_mem,
            pattern: *const c_void,
            pattern_size: usize,
            offset: usize,
            size: usize,
            num_events_in_wait_list: cl_uint,
            event_wait_list: *const cl_event,
            event: *mut cl_event,
        ) -> cl_int,
    >,
    clEnqueueCopyBuffer: unsafe extern "C" fn(
        command_queue: cl_command_queue,
        src_buffer: cl_mem,
        dst_buffer: cl_mem,
        src_offset: usize,
        dst_offset: usize,
        size: usize,
        num_events_in_wait_list: cl_uint,
        event_wait_list: *const cl_event,
        event: *mut cl_event,
    ) -> cl_int,
    clEnqueueMapBuffer: unsafe extern "C" fn(
        command_queue: cl_command_queue,
        buffer: cl_mem,
        blocking_map: cl_bool,
        map_flags: cl_map_flags,
        offset: usize,
        size: usize,
        num_events_in_wait_list: cl_uint,
        event_wait_list: *const cl_event,
        event: *mut cl_event,
        errcode_ret: *mut cl_int,
    ) -> *mut c_void,
    clEnqueueUnmapMemObject: unsafe extern "C" fn(
        command_queue: cl_command_queue,
        memobj: cl_mem,
        mapped_ptr: *mut c_void,
        num_events_in_wait_list: cl_uint,
        event_wait_list: *const cl_event,
        event: *mut cl_event,
    ) -> cl_int,
    clCreateProgramWithSource: unsafe extern "C" fn(
        context: cl_context,
        count: cl_uint,
        strings: *const *const std::os::raw::c_char,
        lengths: *const usize,
        errcode_ret: *mut cl_int,
    ) -> cl_program,
    clBuildProgram: unsafe extern "C" fn(
        program: cl_program,
        num_devices: cl_uint,
        device_list: *const cl_device_id,
        options: *const std::os::raw::c_char,
        pfn_notify: Option<unsafe extern "C" fn(cl_program, *mut c_void)>,
        user_data: *mut c_void,
    ) -> cl_int,
    clGetProgramBuildInfo: unsafe extern "C" fn(
        program: cl_program,
        device: cl_device_id,
        param_name: cl_program_build_info,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> cl_int,
    clReleaseProgram: unsafe extern "C" fn(program: cl_program) -> cl_int,
    clCreateKernel: unsafe extern "C" fn(
        program: cl_program,
        kernel_name: *const std::os::raw::c_char,
        errcode_ret: *mut cl_int,
    ) -> cl_kernel,
    clReleaseKernel: unsafe extern "C" fn(kernel: cl_kernel) -> cl_int,
    clSetKernelArg: unsafe extern "C" fn(
        kernel: cl_kernel,
        arg_index: cl_uint,
        arg_size: usize,
        arg_value: *const c_void,
    ) -> cl_int,
    clEnqueueNDRangeKernel: unsafe extern "C" fn(
        command_queue: cl_command_queue,
        kernel: cl_kernel,
        work_dim: cl_uint,
        global_work_offset: *const usize,
        global_work_size: *const usize,
        local_work_size: *const usize,
        num_events_in_wait_list: cl_uint,
        event_wait_list: *const cl_event,
        event: *mut cl_event,
    ) -> cl_int,
    clGetKernelWorkGroupInfo: unsafe extern "C" fn(
        kernel: cl_kernel,
        device: cl_device_id,
        param_name: cl_kernel_work_group_info,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> cl_int,
    clWaitForEvents: unsafe extern "C" fn(num_events: cl_uint, event_list: *const cl_event) -> cl_int,
    clGetEventInfo: unsafe extern "C" fn(
        event: cl_event,
        param_name: cl_event_info,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> cl_int,
    clReleaseEvent: unsafe extern "C" fn(event: cl_event) -> cl_int,
    clCreateUserEvent: unsafe extern "C" fn(context: cl_context, errcode_ret: *mut cl_int) -> cl_event,
    clSetUserEventStatus: unsafe extern "C" fn(event: cl_event, execution_status: cl_int) -> cl_int,
    clFinish: unsafe extern "C" fn(command_queue: cl_command_queue) -> cl_int,
    clFlush: unsafe extern "C" fn(command_queue: cl_command_queue) -> cl_int,
    // OpenCL 1.1 / 1.2 marker / barrier / callback
    clEnqueueMarkerWithWaitList: Option<
        unsafe extern "C" fn(
            command_queue: cl_command_queue,
            num_events_in_wait_list: cl_uint,
            event_wait_list: *const cl_event,
            event: *mut cl_event,
        ) -> cl_int,
    >,
    clEnqueueBarrierWithWaitList: Option<
        unsafe extern "C" fn(
            command_queue: cl_command_queue,
            num_events_in_wait_list: cl_uint,
            event_wait_list: *const cl_event,
            event: *mut cl_event,
        ) -> cl_int,
    >,
    clSetEventCallback: Option<
        unsafe extern "C" fn(
            event: cl_event,
            command_exec_callback_type: cl_int,
            pfn_notify: ClEventCallback,
            user_data: *mut c_void,
        ) -> cl_int,
    >,
}

// SAFETY: OpenCL function pointers resolved from the dynamically loaded ICD
// library are process-wide and thread-safe for concurrent FFI invocation.
unsafe impl Send for OpenClFunctions {}
// SAFETY: as above.
unsafe impl Sync for OpenClFunctions {}

static OPENCL_LIB: OnceLock<Option<OpenClFunctions>> = OnceLock::new();

fn get_opencl() -> Option<&'static OpenClFunctions> {
    OPENCL_LIB
        .get_or_init(|| {
            let candidates: &[&str] = if cfg!(target_os = "windows") {
                &["OpenCL.dll"]
            } else if cfg!(target_os = "macos") {
                &[
                    "/System/Library/Frameworks/OpenCL.framework/OpenCL",
                    "libOpenCL.dylib",
                ]
            } else {
                &["libOpenCL.so.1", "libOpenCL.so"]
            };

            for &name in candidates {
                // SAFETY: probing a standard OpenCL ICD loader library by known filename.
                if let Ok(lib) = unsafe { Library::new(name) } {
                    if let Some(funcs) = load_symbols(lib) {
                        return Some(funcs);
                    }
                }
            }
            None
        })
        .as_ref()
}

fn load_symbols(lib: Library) -> Option<OpenClFunctions> {
    // SAFETY: `lib` stays owned inside `OpenClFunctions` for `'static`, so
    // the loaded symbols remain valid for the process lifetime and match the
    // standard OpenCL C ABI.
    unsafe {
        let clGetPlatformIDs: Symbol<unsafe extern "C" fn(cl_uint, *mut cl_platform_id, *mut cl_uint) -> cl_int> =
            lib.get(b"clGetPlatformIDs\0").ok()?;
        let clGetPlatformInfo: Symbol<unsafe extern "C" fn(cl_platform_id, cl_platform_info, usize, *mut c_void, *mut usize) -> cl_int> =
            lib.get(b"clGetPlatformInfo\0").ok()?;
        let clGetDeviceIDs: Symbol<unsafe extern "C" fn(cl_platform_id, cl_device_type, cl_uint, *mut cl_device_id, *mut cl_uint) -> cl_int> =
            lib.get(b"clGetDeviceIDs\0").ok()?;
        let clGetDeviceInfo: Symbol<unsafe extern "C" fn(cl_device_id, cl_device_info, usize, *mut c_void, *mut usize) -> cl_int> =
            lib.get(b"clGetDeviceInfo\0").ok()?;
        let clCreateContext: Symbol<unsafe extern "C" fn(*const isize, cl_uint, *const cl_device_id, Option<unsafe extern "C" fn(*const std::os::raw::c_char, *const c_void, usize, *mut c_void)>, *mut c_void, *mut cl_int) -> cl_context> =
            lib.get(b"clCreateContext\0").ok()?;
        let clReleaseContext: Symbol<unsafe extern "C" fn(cl_context) -> cl_int> =
            lib.get(b"clReleaseContext\0").ok()?;
        let clCreateCommandQueue: Symbol<unsafe extern "C" fn(cl_context, cl_device_id, cl_command_queue_properties, *mut cl_int) -> cl_command_queue> =
            lib.get(b"clCreateCommandQueue\0").ok()?;
        let clReleaseCommandQueue: Symbol<unsafe extern "C" fn(cl_command_queue) -> cl_int> =
            lib.get(b"clReleaseCommandQueue\0").ok()?;
        let clCreateBuffer: Symbol<unsafe extern "C" fn(cl_context, cl_mem_flags, usize, *mut c_void, *mut cl_int) -> cl_mem> =
            lib.get(b"clCreateBuffer\0").ok()?;
        let clCreateSubBuffer = lib
            .get::<unsafe extern "C" fn(cl_mem, cl_mem_flags, cl_uint, *const c_void, *mut cl_int) -> cl_mem>(
                b"clCreateSubBuffer\0",
            )
            .ok()
            .map(|s| *s);
        let clRetainMemObject = lib
            .get::<unsafe extern "C" fn(cl_mem) -> cl_int>(b"clRetainMemObject\0")
            .ok()
            .map(|s| *s);
        let clReleaseMemObject: Symbol<unsafe extern "C" fn(cl_mem) -> cl_int> =
            lib.get(b"clReleaseMemObject\0").ok()?;
        let clEnqueueReadBuffer: Symbol<unsafe extern "C" fn(cl_command_queue, cl_mem, cl_bool, usize, usize, *mut c_void, cl_uint, *const cl_event, *mut cl_event) -> cl_int> =
            lib.get(b"clEnqueueReadBuffer\0").ok()?;
        let clEnqueueWriteBuffer: Symbol<unsafe extern "C" fn(cl_command_queue, cl_mem, cl_bool, usize, usize, *const c_void, cl_uint, *const cl_event, *mut cl_event) -> cl_int> =
            lib.get(b"clEnqueueWriteBuffer\0").ok()?;
        let clEnqueueCopyBuffer: Symbol<unsafe extern "C" fn(cl_command_queue, cl_mem, cl_mem, usize, usize, usize, cl_uint, *const cl_event, *mut cl_event) -> cl_int> =
            lib.get(b"clEnqueueCopyBuffer\0").ok()?;
        let clEnqueueMapBuffer: Symbol<unsafe extern "C" fn(cl_command_queue, cl_mem, cl_bool, cl_map_flags, usize, usize, cl_uint, *const cl_event, *mut cl_event, *mut cl_int) -> *mut c_void> =
            lib.get(b"clEnqueueMapBuffer\0").ok()?;
        let clEnqueueUnmapMemObject: Symbol<unsafe extern "C" fn(cl_command_queue, cl_mem, *mut c_void, cl_uint, *const cl_event, *mut cl_event) -> cl_int> =
            lib.get(b"clEnqueueUnmapMemObject\0").ok()?;
        let clCreateProgramWithSource: Symbol<unsafe extern "C" fn(cl_context, cl_uint, *const *const std::os::raw::c_char, *const usize, *mut cl_int) -> cl_program> =
            lib.get(b"clCreateProgramWithSource\0").ok()?;
        let clBuildProgram: Symbol<unsafe extern "C" fn(program: cl_program, cl_uint, *const cl_device_id, *const std::os::raw::c_char, Option<unsafe extern "C" fn(cl_program, *mut c_void)>, *mut c_void) -> cl_int> =
            lib.get(b"clBuildProgram\0").ok()?;
        let clGetProgramBuildInfo: Symbol<unsafe extern "C" fn(cl_program, cl_device_id, cl_program_build_info, usize, *mut c_void, *mut usize) -> cl_int> =
            lib.get(b"clGetProgramBuildInfo\0").ok()?;
        let clReleaseProgram: Symbol<unsafe extern "C" fn(cl_program) -> cl_int> =
            lib.get(b"clReleaseProgram\0").ok()?;
        let clCreateKernel: Symbol<unsafe extern "C" fn(cl_program, *const std::os::raw::c_char, *mut cl_int) -> cl_kernel> =
            lib.get(b"clCreateKernel\0").ok()?;
        let clReleaseKernel: Symbol<unsafe extern "C" fn(cl_kernel) -> cl_int> =
            lib.get(b"clReleaseKernel\0").ok()?;
        let clSetKernelArg: Symbol<unsafe extern "C" fn(cl_kernel, cl_uint, usize, *const c_void) -> cl_int> =
            lib.get(b"clSetKernelArg\0").ok()?;
        let clEnqueueNDRangeKernel: Symbol<unsafe extern "C" fn(cl_command_queue, cl_kernel, cl_uint, *const usize, *const usize, *const usize, cl_uint, *const cl_event, *mut cl_event) -> cl_int> =
            lib.get(b"clEnqueueNDRangeKernel\0").ok()?;
        let clGetKernelWorkGroupInfo: Symbol<unsafe extern "C" fn(cl_kernel, cl_device_id, cl_kernel_work_group_info, usize, *mut c_void, *mut usize) -> cl_int> =
            lib.get(b"clGetKernelWorkGroupInfo\0").ok()?;
        let clWaitForEvents: Symbol<unsafe extern "C" fn(cl_uint, *const cl_event) -> cl_int> =
            lib.get(b"clWaitForEvents\0").ok()?;
        let clGetEventInfo: Symbol<unsafe extern "C" fn(cl_event, cl_event_info, usize, *mut c_void, *mut usize) -> cl_int> =
            lib.get(b"clGetEventInfo\0").ok()?;
        let clReleaseEvent: Symbol<unsafe extern "C" fn(cl_event) -> cl_int> =
            lib.get(b"clReleaseEvent\0").ok()?;
        let clCreateUserEvent: Symbol<unsafe extern "C" fn(cl_context, *mut cl_int) -> cl_event> =
            lib.get(b"clCreateUserEvent\0").ok()?;
        let clSetUserEventStatus: Symbol<unsafe extern "C" fn(cl_event, cl_int) -> cl_int> =
            lib.get(b"clSetUserEventStatus\0").ok()?;
        let clFinish: Symbol<unsafe extern "C" fn(cl_command_queue) -> cl_int> =
            lib.get(b"clFinish\0").ok()?;
        let clFlush: Symbol<unsafe extern "C" fn(cl_command_queue) -> cl_int> =
            lib.get(b"clFlush\0").ok()?;

        let clEnqueueMarkerWithWaitList = lib
            .get(b"clEnqueueMarkerWithWaitList\0")
            .ok()
            .map(|s| *s);
        let clEnqueueBarrierWithWaitList = lib
            .get(b"clEnqueueBarrierWithWaitList\0")
            .ok()
            .map(|s| *s);
        let clSetEventCallback = lib.get(b"clSetEventCallback\0").ok().map(|s| *s);
        let clEnqueueFillBuffer = lib.get(b"clEnqueueFillBuffer\0").ok().map(|s| *s);

        Some(OpenClFunctions {
            clGetPlatformIDs: *clGetPlatformIDs,
            clGetPlatformInfo: *clGetPlatformInfo,
            clGetDeviceIDs: *clGetDeviceIDs,
            clGetDeviceInfo: *clGetDeviceInfo,
            clCreateContext: *clCreateContext,
            clReleaseContext: *clReleaseContext,
            clCreateCommandQueue: *clCreateCommandQueue,
            clReleaseCommandQueue: *clReleaseCommandQueue,
            clCreateBuffer: *clCreateBuffer,
            clCreateSubBuffer,
            clRetainMemObject,
            clReleaseMemObject: *clReleaseMemObject,
            clEnqueueReadBuffer: *clEnqueueReadBuffer,
            clEnqueueWriteBuffer: *clEnqueueWriteBuffer,
            clEnqueueCopyBuffer: *clEnqueueCopyBuffer,
            clEnqueueMapBuffer: *clEnqueueMapBuffer,
            clEnqueueUnmapMemObject: *clEnqueueUnmapMemObject,
            clCreateProgramWithSource: *clCreateProgramWithSource,
            clBuildProgram: *clBuildProgram,
            clGetProgramBuildInfo: *clGetProgramBuildInfo,
            clReleaseProgram: *clReleaseProgram,
            clCreateKernel: *clCreateKernel,
            clReleaseKernel: *clReleaseKernel,
            clSetKernelArg: *clSetKernelArg,
            clEnqueueNDRangeKernel: *clEnqueueNDRangeKernel,
            clGetKernelWorkGroupInfo: *clGetKernelWorkGroupInfo,
            clWaitForEvents: *clWaitForEvents,
            clGetEventInfo: *clGetEventInfo,
            clReleaseEvent: *clReleaseEvent,
            clCreateUserEvent: *clCreateUserEvent,
            clSetUserEventStatus: *clSetUserEventStatus,
            clFinish: *clFinish,
            clFlush: *clFlush,
            clEnqueueMarkerWithWaitList,
            clEnqueueBarrierWithWaitList,
            clSetEventCallback,
            clEnqueueFillBuffer,
            _lib: lib,
        })
    }
}

macro_rules! require_opencl {
    () => {
        let Some(cl) = get_opencl() else {
            return Err(DeviceError::NoDriver);
        };
    };
    ($cl:ident) => {
        let Some($cl) = get_opencl() else {
            return Err(DeviceError::NoDriver);
        };
    };
}

fn cl_err(what: &str, code: cl_int) -> DeviceError {
    let name = match code {
        CL_SUCCESS => "CL_SUCCESS",
        CL_DEVICE_NOT_FOUND => "CL_DEVICE_NOT_FOUND",
        CL_DEVICE_NOT_AVAILABLE => "CL_DEVICE_NOT_AVAILABLE",
        CL_BUILD_PROGRAM_FAILURE => "CL_BUILD_PROGRAM_FAILURE",
        -3 => "CL_COMPILER_NOT_AVAILABLE",
        -4 => "CL_MEM_OBJECT_ALLOCATION_FAILURE",
        -5 => "CL_OUT_OF_RESOURCES",
        -6 => "CL_OUT_OF_HOST_MEMORY",
        -30 => "CL_INVALID_VALUE",
        -33 => "CL_INVALID_DEVICE",
        -34 => "CL_INVALID_CONTEXT",
        -36 => "CL_INVALID_COMMAND_QUEUE",
        -38 => "CL_INVALID_MEM_OBJECT",
        -46 => "CL_INVALID_KERNEL_NAME",
        -48 => "CL_INVALID_KERNEL",
        -49 => "CL_INVALID_ARG_INDEX",
        -50 => "CL_INVALID_ARG_VALUE",
        -51 => "CL_INVALID_ARG_SIZE",
        -52 => "CL_INVALID_KERNEL_ARGS",
        -53 => "CL_INVALID_WORK_DIMENSION",
        -54 => "CL_INVALID_WORK_GROUP_SIZE",
        -55 => "CL_INVALID_WORK_ITEM_SIZE",
        -56 => "CL_INVALID_GLOBAL_OFFSET",
        -57 => "CL_INVALID_EVENT_WAIT_LIST",
        -58 => "CL_INVALID_EVENT",
        other => return DeviceError::Driver(format!("{what} failed: OpenCL error {other}")),
    };
    DeviceError::Driver(format!("{what} failed: {name} ({code})"))
}

fn check(what: &str, code: cl_int) -> Result<()> {
    if code == CL_SUCCESS {
        Ok(())
    } else {
        Err(cl_err(what, code))
    }
}

// ── OpenCL Discovery & Probing ───────────────────────────────────────

fn get_string_device_info(
    cl: &OpenClFunctions,
    device: cl_device_id,
    param: cl_device_info,
) -> Result<String> {
    let mut size: usize = 0;
    // SAFETY: `device` is a live OpenCL device handle and `size`/`buf` out-params
    // are valid for the duration of the queries.
    unsafe {
        check(
            "clGetDeviceInfo(size)",
            (cl.clGetDeviceInfo)(device, param, 0, std::ptr::null_mut(), &mut size),
        )?;
        if size == 0 {
            return Ok(String::new());
        }
        let mut buf = vec![0u8; size];
        check(
            "clGetDeviceInfo(data)",
            (cl.clGetDeviceInfo)(device, param, size, buf.as_mut_ptr() as *mut c_void, std::ptr::null_mut()),
        )?;
        // Strip trailing NUL bytes
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }
}

fn get_string_platform_info(
    cl: &OpenClFunctions,
    platform: cl_platform_id,
    param: cl_platform_info,
) -> Result<String> {
    let mut size: usize = 0;
    // SAFETY: `platform` is a live OpenCL platform handle and `size`/`buf`
    // out-params are valid for the duration of the queries.
    unsafe {
        check(
            "clGetPlatformInfo(size)",
            (cl.clGetPlatformInfo)(platform, param, 0, std::ptr::null_mut(), &mut size),
        )?;
        if size == 0 {
            return Ok(String::new());
        }
        let mut buf = vec![0u8; size];
        check(
            "clGetPlatformInfo(data)",
            (cl.clGetPlatformInfo)(platform, param, size, buf.as_mut_ptr() as *mut c_void, std::ptr::null_mut()),
        )?;
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }
}

fn parse_opencl_version(s: &str) -> (u32, u32) {
    // Expected format: "OpenCL <major>.<minor> ..."
    let text = s.trim();
    let parts: Vec<&str> = text.split_whitespace().collect();
    for p in parts {
        let mut sub = p.split('.');
        if let (Some(maj), Some(min)) = (sub.next(), sub.next()) {
            if let (Ok(m1), Ok(m2)) = (maj.parse::<u32>(), min.parse::<u32>()) {
                return (m1, m2);
            }
        }
    }
    (1, 2)
}

struct ProbedDevice {
    platform: cl_platform_id,
    device: cl_device_id,
    caps: DeviceCaps,
}

fn probe_all_devices(cl: &OpenClFunctions) -> Result<Vec<ProbedDevice>> {
    let mut num_platforms: cl_uint = 0;
    // SAFETY: `num_platforms` is a valid aligned out-parameter.
    unsafe {
        check(
            "clGetPlatformIDs(count)",
            (cl.clGetPlatformIDs)(0, std::ptr::null_mut(), &mut num_platforms),
        )?;
    }
    if num_platforms == 0 {
        return Ok(Vec::new());
    }
    let mut platforms = vec![std::ptr::null_mut(); num_platforms as usize];
    // SAFETY: `platforms` has capacity for `num_platforms` handles.
    unsafe {
        check(
            "clGetPlatformIDs",
            (cl.clGetPlatformIDs)(num_platforms, platforms.as_mut_ptr(), std::ptr::null_mut()),
        )?;
    }

    let mut result = Vec::new();
    let mut ordinal = 0u32;

    for &platform in &platforms {
        let platform_name = get_string_platform_info(cl, platform, CL_PLATFORM_NAME)
            .unwrap_or_else(|_| "Unknown Platform".into());

        let mut num_devs: cl_uint = 0;
        // Search GPU first; fall back to ALL
        // SAFETY: `platform` is live and `num_devs` is a valid out-param.
        let err = unsafe {
            (cl.clGetDeviceIDs)(
                platform,
                CL_DEVICE_TYPE_GPU | CL_DEVICE_TYPE_ACCELERATOR | CL_DEVICE_TYPE_DEFAULT,
                0,
                std::ptr::null_mut(),
                &mut num_devs,
            )
        };
        if err != CL_SUCCESS || num_devs == 0 {
            continue;
        }

        let mut devices = vec![std::ptr::null_mut(); num_devs as usize];
        // SAFETY: `devices` has length `num_devs` matching the query above.
        let err = unsafe {
            (cl.clGetDeviceIDs)(
                platform,
                CL_DEVICE_TYPE_GPU | CL_DEVICE_TYPE_ACCELERATOR | CL_DEVICE_TYPE_DEFAULT,
                num_devs,
                devices.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if err != CL_SUCCESS {
            continue;
        }

        for &dev in &devices {
            let name = get_string_device_info(cl, dev, CL_DEVICE_NAME)
                .unwrap_or_else(|_| "OpenCL Device".into());
            let dev_version = get_string_device_info(cl, dev, CL_DEVICE_VERSION)
                .unwrap_or_else(|_| "OpenCL 1.2".into());
            let driver_version = get_string_device_info(cl, dev, CL_DRIVER_VERSION)
                .unwrap_or_else(|_| "1.0".into());
            let extensions = get_string_device_info(cl, dev, CL_DEVICE_EXTENSIONS)
                .unwrap_or_default();

            let mut compute_units: cl_uint = 0;
            let mut max_work_group: usize = 256;
            let mut global_mem: cl_ulong = 0;
            let mut local_mem: cl_ulong = 0;

            // SAFETY: each scalar out-param matches the documented OpenCL query size.
            unsafe {
                let _ = (cl.clGetDeviceInfo)(
                    dev,
                    CL_DEVICE_MAX_COMPUTE_UNITS,
                    std::mem::size_of::<cl_uint>(),
                    &mut compute_units as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
                let _ = (cl.clGetDeviceInfo)(
                    dev,
                    CL_DEVICE_MAX_WORK_GROUP_SIZE,
                    std::mem::size_of::<usize>(),
                    &mut max_work_group as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
                let _ = (cl.clGetDeviceInfo)(
                    dev,
                    CL_DEVICE_GLOBAL_MEM_SIZE,
                    std::mem::size_of::<cl_ulong>(),
                    &mut global_mem as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
                let _ = (cl.clGetDeviceInfo)(
                    dev,
                    CL_DEVICE_LOCAL_MEM_SIZE,
                    std::mem::size_of::<cl_ulong>(),
                    &mut local_mem as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }

            let fp64 = extensions.contains("cl_khr_fp64");
            let int64_atomics = extensions.contains("cl_khr_int64_base_atomics");
            let opencl_version = parse_opencl_version(&dev_version);
            let vendor = get_string_device_info(cl, dev, CL_DEVICE_VENDOR).unwrap_or_default();
            let host_unified_memory = query_cl_uint(cl, dev, CL_DEVICE_HOST_UNIFIED_MEMORY)
                .is_some_and(|v| v != 0);
            let svm_capabilities = query_cl_ulong(cl, dev, CL_DEVICE_SVM_CAPABILITIES).unwrap_or(0);
            let wavefront_attr = query_cl_uint(cl, dev, CL_DEVICE_WAVEFRONT_WIDTH_AMD);
            let gfxip_major = query_cl_uint(cl, dev, CL_DEVICE_GFXIP_MAJOR_AMD);
            let warp_size = amd_opencl_warp_size(wavefront_attr, gfxip_major, &vendor, &name);
            let shared_memory =
                shared_memory_caps_from_opencl(host_unified_memory, svm_capabilities);

            let ocl_caps = OpenClCaps {
                platform_name: platform_name.clone(),
                device_name: name.clone(),
                device_version: dev_version,
                driver_version,
                opencl_version,
                max_compute_units: compute_units,
                max_work_group_size: max_work_group,
                global_mem_size: global_mem,
                local_mem_size: local_mem,
                fp64,
                global_int64_atomics: int64_atomics,
                warp_size,
                host_unified_memory,
                svm_capabilities,
            };

            let caps = DeviceCaps {
                ordinal,
                name,
                compute_major: opencl_version.0,
                compute_minor: opencl_version.1,
                total_global_mem: global_mem,
                arch: DeviceArch::OpenCl(ocl_caps),
                shared_memory,
            };

            result.push(ProbedDevice {
                platform,
                device: dev,
                caps,
            });
            ordinal += 1;
        }
    }

    Ok(result)
}

fn query_cl_uint(cl: &OpenClFunctions, dev: cl_device_id, param: cl_device_info) -> Option<u32> {
    let mut val: cl_uint = 0;
    // SAFETY: `val` is a live out-param matching `cl_uint`.
    let rc = unsafe {
        (cl.clGetDeviceInfo)(
            dev,
            param,
            std::mem::size_of::<cl_uint>(),
            &mut val as *mut _ as *mut c_void,
            std::ptr::null_mut(),
        )
    };
    (rc == CL_SUCCESS).then_some(val)
}

fn query_cl_ulong(cl: &OpenClFunctions, dev: cl_device_id, param: cl_device_info) -> Option<u64> {
    let mut val: cl_ulong = 0;
    // SAFETY: `val` is a live out-param matching `cl_ulong`.
    let rc = unsafe {
        (cl.clGetDeviceInfo)(
            dev,
            param,
            std::mem::size_of::<cl_ulong>(),
            &mut val as *mut _ as *mut c_void,
            std::ptr::null_mut(),
        )
    };
    (rc == CL_SUCCESS).then_some(val)
}

pub(crate) fn shared_memory_caps_from_opencl(
    host_unified_memory: bool,
    svm_capabilities: u64,
) -> SharedMemoryCaps {
    const CL_DEVICE_SVM_FINE_GRAIN_SYSTEM: u64 = 1 << 2;
    SharedMemoryCaps {
        integrated: host_unified_memory,
        can_map_host_memory: host_unified_memory || svm_capabilities != 0,
        unified_addressing: svm_capabilities != 0,
        pageable_memory_access: (svm_capabilities & CL_DEVICE_SVM_FINE_GRAIN_SYSTEM) != 0,
        managed_memory: svm_capabilities != 0,
        host_register_supported: true,
    }
}

pub(crate) fn amd_opencl_warp_size(
    wavefront_width_attr: Option<u32>,
    gfxip_major: Option<u32>,
    vendor: &str,
    name: &str,
) -> u32 {
    let v = vendor.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    if v.contains("amd") || n.contains("amd") || n.contains("radeon") || n.starts_with("gfx") {
        if let Some(wf) = wavefront_width_attr.filter(|&w| w == 32 || w == 64) {
            return wf;
        }
        if let Some(major) = gfxip_major {
            return if major >= 10 { 32 } else { 64 };
        }
        for rdna_hint in [
            "gfx10", "gfx11", "gfx12", "780m", "760m", "740m", "880m", "890m",
            "8050s", "8060s", "strix", "phoenix", "hawk", "krackan", "radeon 7", "radeon 8",
        ] {
            if n.contains(rdna_hint) {
                return 32;
            }
        }
        64
    } else {
        32
    }
}

pub(crate) fn probe_device(device_ordinal: u32) -> Result<DeviceCaps> {
    require_opencl!(cl);
    let devs = probe_all_devices(cl)?;
    devs.into_iter()
        .find(|d| d.caps.ordinal == device_ordinal)
        .map(|d| d.caps)
        .ok_or(DeviceError::NoDriver)
}

pub(crate) fn toolchain_version() -> Result<crate::ToolchainVersion> {
    require_opencl!(cl);
    let devs = probe_all_devices(cl)?;
    if let Some(first) = devs.first() {
        if let DeviceArch::OpenCl(caps) = &first.caps.arch {
            return Ok(crate::ToolchainVersion::OpenCl {
                major: caps.opencl_version.0,
                minor: caps.opencl_version.1,
            });
        }
    }
    Err(DeviceError::NoDriver)
}

pub(crate) fn driver_cuda_version() -> Result<u32> {
    Err(DeviceError::Driver(
        "OpenCL backend has no CUDA driver version".into(),
    ))
}

// ── Context and Device ────────────────────────────────────────────────

pub(crate) struct OpenClDevice {
    pub(crate) ordinal: u32,
    pub(crate) platform: cl_platform_id,
    pub(crate) device_id: cl_device_id,
    pub(crate) context: cl_context,
    pub(crate) default_queue: cl_command_queue,
    pub(crate) max_work_group: usize,
    pub(crate) shared_memory: SharedMemoryCaps,
    pub(crate) user_queues: Mutex<Vec<std::sync::Weak<OpenClStream>>>,
}

// SAFETY: OpenCL handles live inside the OpenCL driver and are thread-safe to
// pass across threads under our synchronization discipline.
unsafe impl Send for OpenClDevice {}
// SAFETY: as above.
unsafe impl Sync for OpenClDevice {}

impl Drop for OpenClDevice {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
            // SAFETY: this is the last reference to `OpenClDevice` and both
            // handles were created in `DeviceContextInner::new`.
            unsafe {
                let _ = (cl.clReleaseCommandQueue)(self.default_queue);
                let _ = (cl.clReleaseContext)(self.context);
            }
        }
    }
}

pub(crate) struct OpenClStream {
    pub(crate) raw: cl_command_queue,
    pub(crate) device: Arc<OpenClDevice>,
}

// SAFETY: OpenCL command queues are thread-safe to transfer across threads.
unsafe impl Send for OpenClStream {}
// SAFETY: OpenCL command queues serialize submissions internally or per queue.
unsafe impl Sync for OpenClStream {}

impl Drop for OpenClStream {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
            // SAFETY: `self.raw` is uniquely owned by this `OpenClStream`.
            unsafe {
                let _ = (cl.clReleaseCommandQueue)(self.raw);
            }
        }
    }
}

// ── Event Handle ──────────────────────────────────────────────────────

pub struct OpenClEventHandle {
    pub(crate) cl_event: Mutex<Option<cl_event>>,
    pub(crate) device: Arc<OpenClDevice>,
}

// SAFETY: `cl_event` is protected by a `Mutex` and `context` is immutable.
unsafe impl Send for OpenClEventHandle {}
// SAFETY: as above.
unsafe impl Sync for OpenClEventHandle {}

thread_local! {
    static CURRENT_DEVICE: std::cell::RefCell<Option<Arc<OpenClDevice>>> = const { std::cell::RefCell::new(None) };
}

// ── Event Pool ────────────────────────────────────────────────────────

const EVENT_POOL_CAP: usize = 4096;

impl crate::backend_api::RawEventDestroy<*mut OpenClEventHandle> for OpenClDevice {
    fn is_null(ev: &*mut OpenClEventHandle) -> bool {
        ev.is_null()
    }
    fn bind_for_destroy(&self) {}
    unsafe fn destroy_event(&self, ev: *mut OpenClEventHandle) {
        unsafe {
            <OpenClBackend as crate::backend_api::DriverApi>::event_destroy(ev);
        }
    }
}

pub(crate) type EventPool =
    crate::backend_api::RawEventPool<*mut OpenClEventHandle, OpenClDevice>;

// ── Suballocator Backend ──────────────────────────────────────────────

#[inline]
fn align_up_ocl(value: usize, align: usize) -> usize {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value.saturating_add(align - rem)
    }
}

struct OpenClSlabHandle {
    base_va: u64,
    mem: usize,
}

struct OpenClSlabBackend {
    context: cl_context,
    sub_align: usize,
    next_va: AtomicU64,
    slabs: Mutex<BTreeMap<u64, (usize, usize)>>,
}

// SAFETY: `context` outlives `OpenClSubAllocator` via `OpenClDevice` and
// OpenCL memory objects are thread-safe to create/release across threads.
unsafe impl Send for OpenClSlabBackend {}
// SAFETY: mutable state (`slabs`) is guarded by a `Mutex` and `next_va` is atomic.
unsafe impl Sync for OpenClSlabBackend {}

impl OpenClSlabBackend {
    fn new(context: cl_context, sub_align: usize) -> Self {
        Self {
            context,
            sub_align: sub_align.max(MIN_BLOCK_ALIGN),
            next_va: AtomicU64::new(0x0001_0000_0000_0000),
            slabs: Mutex::new(BTreeMap::new()),
        }
    }

    fn acquire_mem_for_lease(&self, lease: &AllocLease) -> Result<(cl_mem, bool)> {
        require_opencl!(cl);
        let (base_va, parent_raw, slab_size) = {
            let guard = self.slabs.lock().unwrap_or_else(|p| p.into_inner());
            let (&base_va, &(parent_raw, slab_size)) = guard
                .range(..=lease.ptr)
                .next_back()
                .ok_or_else(|| DeviceError::Driver("OpenCL suballocator slab missing".into()))?;
            (base_va, parent_raw, slab_size)
        };
        let origin = (lease.ptr - base_va) as usize;
        let parent = parent_raw as cl_mem;
        if origin == 0 && lease.alloc_bytes == slab_size {
            if let Some(retain_fn) = cl.clRetainMemObject {
                // SAFETY: `parent` is a live buffer object kept alive by `self.slabs`.
                check("clRetainMemObject", unsafe { retain_fn(parent) })?;
                return Ok((parent, true));
            }
            return Ok((parent, false));
        }
        let create_sub = cl.clCreateSubBuffer.ok_or_else(|| {
            DeviceError::Driver("clCreateSubBuffer is unavailable on this OpenCL runtime".into())
        })?;
        let region = ClBufferRegion {
            origin,
            size: lease.alloc_bytes,
        };
        let mut err: cl_int = 0;
        // SAFETY: `parent` is a live `CL_MEM_READ_WRITE` buffer and `region`
        // describes an aligned in-bounds sub-region of it.
        let sub = unsafe {
            create_sub(
                parent,
                CL_MEM_READ_WRITE,
                CL_BUFFER_CREATE_TYPE_REGION,
                &region as *const ClBufferRegion as *const c_void,
                &mut err,
            )
        };
        check("clCreateSubBuffer", err)?;
        Ok((sub, true))
    }
}

impl SlabBackend for OpenClSlabBackend {
    type Slab = OpenClSlabHandle;

    fn alloc_slab(&self, min_bytes: usize) -> Result<BackingSlab<Self::Slab>> {
        require_opencl!(cl);
        let size_bytes = align_up_ocl(min_bytes.max(1), self.sub_align);
        let mut err: cl_int = 0;
        // SAFETY: `self.context` is valid for the lifetime of `OpenClDevice`.
        let mem = unsafe {
            (cl.clCreateBuffer)(
                self.context,
                CL_MEM_READ_WRITE,
                size_bytes,
                std::ptr::null_mut(),
                &mut err,
            )
        };
        check("clCreateBuffer(slab)", err)?;
        let base_va = self.next_va.fetch_add(size_bytes as u64, Ordering::Relaxed);
        self.slabs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(base_va, (mem as usize, size_bytes));
        Ok(BackingSlab {
            base_ptr: base_va,
            size_bytes,
            handle: OpenClSlabHandle {
                base_va,
                mem: mem as usize,
            },
        })
    }

    fn free_slab(&self, slab: BackingSlab<Self::Slab>) {
        self.slabs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&slab.handle.base_va);
        if let Some(cl) = get_opencl() {
            // SAFETY: `SubAllocator` only frees an idle slab once all child leases
            // have retired.
            unsafe {
                let _ = (cl.clReleaseMemObject)(slab.handle.mem as cl_mem);
            }
        }
    }
}

type OpenClSubAllocator = SubAllocator<OpenClSlabBackend, Arc<crate::Event>>;

// ── Context Inner ─────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    device: Arc<OpenClDevice>,
    copy_h2d: Arc<OpenClStream>,
    copy_d2h: Arc<OpenClStream>,
    alloc_pool: Arc<OpenClSubAllocator>,
    sub_align: usize,
    event_pool: Arc<EventPool>,
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        Self::new_for_vm(device_ordinal, 0)
    }

    pub(crate) fn new_for_vm(device_ordinal: u32, vm_identity: u64) -> Result<Self> {
        require_opencl!(cl);
        let devs = probe_all_devices(cl)?;
        let target = devs
            .into_iter()
            .find(|d| d.caps.ordinal == device_ordinal)
            .ok_or(DeviceError::NoDriver)?;

        let mut err: cl_int = 0;
        // SAFETY: `target.device` was returned by `clGetDeviceIDs` and `err` is
        // a valid out-param.
        let context = unsafe {
            (cl.clCreateContext)(
                std::ptr::null(),
                1,
                &target.device,
                None,
                std::ptr::null_mut(),
                &mut err,
            )
        };
        check("clCreateContext", err)?;

        // SAFETY: `context` and `target.device` are valid live handles.
        let default_queue = unsafe {
            (cl.clCreateCommandQueue)(context, target.device, 0, &mut err)
        };
        if err != CL_SUCCESS {
            // SAFETY: cleanup of `context` after queue creation failed.
            unsafe {
                let _ = (cl.clReleaseContext)(context);
            }
            return Err(cl_err("clCreateCommandQueue", err));
        }

        let mut base_align_bits: cl_uint = 0;
        // SAFETY: `base_align_bits` matches `cl_uint` documented size.
        let align_rc = unsafe {
            (cl.clGetDeviceInfo)(
                target.device,
                CL_DEVICE_MEM_BASE_ADDR_ALIGN,
                std::mem::size_of::<cl_uint>(),
                &mut base_align_bits as *mut _ as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        let sub_align = if align_rc == CL_SUCCESS && base_align_bits > 0 {
            ((base_align_bits as usize).div_ceil(8))
                .next_power_of_two()
                .max(MIN_BLOCK_ALIGN)
        } else {
            MIN_BLOCK_ALIGN.max(512)
        };

        let max_work_group = match target.caps.arch {
            DeviceArch::OpenCl(ref o) => o.max_work_group_size,
            _ => 256,
        };

        let device = Arc::new(OpenClDevice {
            ordinal: device_ordinal,
            platform: target.platform,
            device_id: target.device,
            context,
            default_queue,
            max_work_group,
            shared_memory: target.caps.shared_memory,
            user_queues: Mutex::new(Vec::new()),
        });

        // SAFETY: `context` and `target.device` are valid for creating transfer queues.
        let q1 = unsafe { (cl.clCreateCommandQueue)(context, target.device, 0, &mut err) };
        check("clCreateCommandQueue(h2d)", err)?;
        let copy_h2d = Arc::new(OpenClStream {
            raw: q1,
            device: Arc::clone(&device),
        });

        // SAFETY: as above.
        let q2 = unsafe { (cl.clCreateCommandQueue)(context, target.device, 0, &mut err) };
        check("clCreateCommandQueue(d2h)", err)?;
        let copy_d2h = Arc::new(OpenClStream {
            raw: q2,
            device: Arc::clone(&device),
        });

        let can_subbuffer = cl.clCreateSubBuffer.is_some() && cl.clRetainMemObject.is_some();
        let alloc_pool = SubAllocator::new_arc_with_quota(
            OpenClSlabBackend::new(context, sub_align),
            DEFAULT_SLAB_BYTES,
            device_pool_enabled() && can_subbuffer,
            vm_identity,
            device_ordinal,
        );

        let event_pool = Arc::new(EventPool::new(Arc::clone(&device)));

        Ok(Self {
            device,
            copy_h2d,
            copy_d2h,
            alloc_pool,
            sub_align,
            event_pool,
        })
    }

    pub(crate) fn record_alloc_event(&self, event: &crate::Event) -> Result<()> {
        self.bind_to_thread()?;
        event.set_recorded_on(self.copy_h2d.raw);
        // SAFETY: `event` is live and `self.copy_h2d.raw` belongs to this context.
        unsafe {
            <OpenClBackend as crate::backend_api::DriverApi>::event_record(
                event.cu_event_raw(),
                self.copy_h2d.raw,
            )
        }
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        require_opencl!(cl);
        let live_user_queues: Vec<Arc<OpenClStream>> = {
            let mut guard = self.device.user_queues.lock().unwrap_or_else(|p| p.into_inner());
            let mut live = Vec::with_capacity(guard.len());
            guard.retain(|w| {
                if let Some(s) = w.upgrade() {
                    live.push(s);
                    true
                } else {
                    false
                }
            });
            live
        };
        // SAFETY: all command queues are owned by this live context / `Arc<OpenClStream>`.
        unsafe {
            check("clFinish(default)", (cl.clFinish)(self.device.default_queue))?;
            check("clFinish(copy_h2d)", (cl.clFinish)(self.copy_h2d.raw))?;
            check("clFinish(copy_d2h)", (cl.clFinish)(self.copy_d2h.raw))?;
            for q in &live_user_queues {
                check("clFinish(user_queue)", (cl.clFinish)(q.raw))?;
            }
        }
        Ok(())
    }

    pub(crate) fn bind_to_thread(&self) -> Result<()> {
        CURRENT_DEVICE.with(|c| {
            *c.borrow_mut() = Some(Arc::clone(&self.device));
        });
        Ok(())
    }

    pub(crate) fn device(&self) -> &Arc<OpenClDevice> {
        &self.device
    }

    pub(crate) fn copy_h2d(&self) -> &Arc<OpenClStream> {
        &self.copy_h2d
    }

    pub(crate) fn copy_d2h(&self) -> &Arc<OpenClStream> {
        &self.copy_d2h
    }

    pub(crate) fn event_pool(&self) -> &Arc<EventPool> {
        &self.event_pool
    }
}

// ── Module Inner ──────────────────────────────────────────────────────

pub(crate) struct DeviceModuleInner {
    program: cl_program,
    kernels: HashMap<String, Mutex<cl_kernel>>,
    device: Arc<OpenClDevice>,
}

// SAFETY: OpenCL programs and kernels compiled under one context can be sent across threads.
unsafe impl Send for DeviceModuleInner {}
// SAFETY: each `cl_kernel` is serialized by its own `Mutex` across `clSetKernelArg` and launch.
unsafe impl Sync for DeviceModuleInner {}

impl Drop for DeviceModuleInner {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
            // SAFETY: `DeviceModuleInner` owns these kernel and program handles uniquely.
            unsafe {
                for (_, k) in self.kernels.drain() {
                    let _ = (cl.clReleaseKernel)(k.into_inner().unwrap_or_else(|p| p.into_inner()));
                }
                let _ = (cl.clReleaseProgram)(self.program);
            }
        }
    }
}

impl DeviceModuleInner {
    pub(crate) fn from_opencl_source(
        ctx: &DeviceContextInner,
        source: &str,
        module_name: &str,
        kernel_names: &[&'static str],
    ) -> Result<Self> {
        require_opencl!(cl);
        let c_src = CString::new(source)
            .map_err(|_| DeviceError::Load(format!("{module_name}: source contains NUL")))?;
        let src_ptr = c_src.as_ptr();
        let src_len = source.len();

        let mut err: cl_int = 0;
        // SAFETY: `src_ptr` points to NUL-terminated OpenCL C source for `src_len` bytes.
        let program = unsafe {
            (cl.clCreateProgramWithSource)(
                ctx.device.context,
                1,
                &src_ptr,
                &src_len,
                &mut err,
            )
        };
        check("clCreateProgramWithSource", err)?;

        // SAFETY: `program` and `ctx.device.device_id` are valid live handles.
        let build_rc = unsafe {
            (cl.clBuildProgram)(
                program,
                1,
                &ctx.device.device_id,
                std::ptr::null(),
                None,
                std::ptr::null_mut(),
            )
        };
        if build_rc != CL_SUCCESS {
            let mut log_size: usize = 0;
            let mut log = String::new();
            // SAFETY: querying build log size/bytes on the failed `program`, then releasing it.
            unsafe {
                let _ = (cl.clGetProgramBuildInfo)(
                    program,
                    ctx.device.device_id,
                    CL_PROGRAM_BUILD_LOG,
                    0,
                    std::ptr::null_mut(),
                    &mut log_size,
                );
                if log_size > 0 {
                    let mut log_buf = vec![0u8; log_size];
                    let _ = (cl.clGetProgramBuildInfo)(
                        program,
                        ctx.device.device_id,
                        CL_PROGRAM_BUILD_LOG,
                        log_size,
                        log_buf.as_mut_ptr() as *mut c_void,
                        std::ptr::null_mut(),
                    );
                    log = String::from_utf8_lossy(&log_buf).to_string();
                }
                let _ = (cl.clReleaseProgram)(program);
            }
            return Err(DeviceError::Load(format!(
                "{module_name}: OpenCL build failed (code {build_rc}):\n{log}"
            )));
        }

        let mut kernels: HashMap<String, Mutex<cl_kernel>> =
            HashMap::with_capacity(kernel_names.len());
        let release_partial = |kernels: &mut HashMap<String, Mutex<cl_kernel>>| unsafe {
            for (_, k) in kernels.drain() {
                let _ = (cl.clReleaseKernel)(k.into_inner().unwrap_or_else(|p| p.into_inner()));
            }
            let _ = (cl.clReleaseProgram)(program);
        };
        for &kname in kernel_names {
            let Ok(c_kname) = CString::new(kname) else {
                release_partial(&mut kernels);
                return Err(DeviceError::Load(format!("kernel name `{kname}` contains NUL")));
            };
            // SAFETY: `program` was built successfully and `c_kname` is NUL-terminated.
            let kernel = unsafe {
                (cl.clCreateKernel)(program, c_kname.as_ptr(), &mut err)
            };
            if err != CL_SUCCESS {
                release_partial(&mut kernels);
                return Err(cl_err(&format!("clCreateKernel({kname})"), err));
            }
            kernels.insert(kname.to_string(), Mutex::new(kernel));
        }

        Ok(Self {
            program,
            kernels,
            device: Arc::clone(&ctx.device),
        })
    }

    pub(crate) fn optimal_block_size(&self, _ctx: &DeviceContextInner, kernel: &str) -> Option<u32> {
        let cl = get_opencl()?;
        let k = self.kernels.get(kernel)?.lock().unwrap_or_else(|p| p.into_inner());
        let mut size: usize = 0;
        // SAFETY: `*k` and `self.device.device_id` are valid live handles.
        let rc = unsafe {
            (cl.clGetKernelWorkGroupInfo)(
                *k,
                self.device.device_id,
                CL_KERNEL_PREFERRED_WORK_GROUP_SIZE_MULTIPLE,
                std::mem::size_of::<usize>(),
                &mut size as *mut _ as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        if rc == CL_SUCCESS && size > 0 {
            Some(size as u32)
        } else {
            None
        }
    }

    pub(crate) fn launch_raw_on_stream(
        &self,
        _ctx: &DeviceContextInner,
        stream: &Arc<OpenClStream>,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        require_opencl!(cl);
        let k = self
            .kernels
            .get(kernel)
            .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        for (i, arg) in args.raw.iter().enumerate() {
            let idx = i as cl_uint;
            let rc = match arg {
                crate::KernelArg::DevicePtr { addr, .. } => {
                    let mem = *addr as cl_mem;
                    // SAFETY: `mem` is kept alive by `args` for the launch call.
                    unsafe {
                        (cl.clSetKernelArg)(
                            *k,
                            idx,
                            std::mem::size_of::<cl_mem>(),
                            &mem as *const _ as *const c_void,
                        )
                    }
                }
                // SAFETY: `val` is a stack-borrowed primitive matching `arg_size`.
                crate::KernelArg::I32(val) => unsafe {
                    (cl.clSetKernelArg)(
                        *k,
                        idx,
                        std::mem::size_of::<i32>(),
                        val as *const _ as *const c_void,
                    )
                },
                // SAFETY: as above.
                crate::KernelArg::I64(val) => unsafe {
                    (cl.clSetKernelArg)(
                        *k,
                        idx,
                        std::mem::size_of::<i64>(),
                        val as *const _ as *const c_void,
                    )
                },
                // SAFETY: as above.
                crate::KernelArg::F32(val) => unsafe {
                    (cl.clSetKernelArg)(
                        *k,
                        idx,
                        std::mem::size_of::<f32>(),
                        val as *const _ as *const c_void,
                    )
                },
                // SAFETY: as above.
                crate::KernelArg::F64(val) => unsafe {
                    (cl.clSetKernelArg)(
                        *k,
                        idx,
                        std::mem::size_of::<f64>(),
                        val as *const _ as *const c_void,
                    )
                },
            };
            check("clSetKernelArg", rc)?;
        }

        // OpenCL global work size is total threads across all work groups
        let gx = (cfg.grid.0 as usize) * (cfg.block.0 as usize);
        let gy = (cfg.grid.1 as usize) * (cfg.block.1 as usize);
        let gz = (cfg.grid.2 as usize) * (cfg.block.2 as usize);
        let bx = cfg.block.0 as usize;
        let by = cfg.block.1 as usize;
        let bz = cfg.block.2 as usize;

        let (work_dim, global_work_size, local_work_size) = if cfg.grid.2 > 1 || cfg.block.2 > 1 {
            (3, [gx, gy, gz], [bx, by, bz])
        } else if cfg.grid.1 > 1 || cfg.block.1 > 1 {
            (2, [gx, gy, 1], [bx, by, 1])
        } else {
            (1, [gx, 1, 1], [bx, 1, 1])
        };

        // SAFETY: kernel arguments were bound above and `global_work_size` /
        // `local_work_size` arrays match `work_dim`.
        unsafe {
            check(
                "clEnqueueNDRangeKernel",
                (cl.clEnqueueNDRangeKernel)(
                    stream.raw,
                    *k,
                    work_dim,
                    std::ptr::null(),
                    global_work_size.as_ptr(),
                    local_work_size.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                ),
            )?;
        }

        Ok(())
    }
}

// ── Alloc and Buffer Inner ────────────────────────────────────────────

/// Retrieve the raw `cl_event` stored inside an `OpenClEventHandle` if it has
/// already been recorded on a queue.
///
/// # Safety
///
/// If `wait_event` is `Some(ptr)` with non-null `ptr`, `ptr` must point to a
/// live [`OpenClEventHandle`] for the duration of this call.
unsafe fn extract_cl_event(wait_event: Option<*mut OpenClEventHandle>) -> Option<cl_event> {
    wait_event.and_then(|h| {
        if h.is_null() {
            None
        } else {
            // SAFETY: caller guarantees `h` points to a live `OpenClEventHandle`.
            let lock = unsafe { (*h).cl_event.lock().unwrap_or_else(|p| p.into_inner()) };
            lock.filter(|ev| !ev.is_null())
        }
    })
}

struct ManagedAlloc {
    mem: cl_mem,
    owns_mem: bool,
    shared_host_mapped: bool,
    host_ptr: Option<usize>,
    _keep_alive: Option<BufferKeepAlive>,
    lease: Option<AllocLease>,
    pool: Arc<OpenClSubAllocator>,
    ctx: DeviceContextInner,
    retire: Mutex<RetireState>,
}

// SAFETY: `cl_mem` is thread-safe in OpenCL and `lease` is exclusively owned by
// `ManagedAlloc` until retired to `pool`.
unsafe impl Send for ManagedAlloc {}
// SAFETY: mutable state (`retire`) is guarded by a `Mutex`.
unsafe impl Sync for ManagedAlloc {}

impl ManagedAlloc {
    fn set_fence(&self, stream_id: Option<u64>, event: Option<Arc<crate::Event>>) {
        RetireState::set_fence(&self.retire, stream_id, event);
    }

    fn add_fence(&self, stream_id: Option<u64>, event: Option<Arc<crate::Event>>) {
        RetireState::add_fence(&self.retire, stream_id, event);
    }

    fn set_idle(&self) {
        RetireState::set_idle(&self.retire);
    }
}

impl Drop for ManagedAlloc {
    fn drop(&mut self) {
        let lease = self.lease.take();
        if lease.is_none() {
            // Standalone shared/host-mapped allocations (`CL_MEM_USE_HOST_PTR` /
            // `CL_MEM_ALLOC_HOST_PTR`) must retire queued GPU work BEFORE we
            // release the cl_mem object or drop `_keep_alive` (`PinnedSpanGuard`).
            RetireState::sync_if_busy(&self.retire, || {
                let _ = self.ctx.synchronize();
            });
        }
        if self.owns_mem && !self.mem.is_null() {
            if let Some(cl) = get_opencl() {
                // SAFETY: `self.mem` is uniquely owned; standalone host-aliased
                // buffers synchronized above and pooled buffers wait via `retire_lease`.
                unsafe {
                    let _ = (cl.clReleaseMemObject)(self.mem);
                }
            }
        }
        if let Some(lease) = lease {
            RetireState::retire_lease(&self.retire, &self.pool, lease, || {
                let _ = self.ctx.synchronize();
            });
        }
    }
}

/// Stamp the latest stream/event fence onto a type-erased [`BufferKeepAlive`].
pub(crate) fn note_keep_alive_fence(
    keep_alive: &BufferKeepAlive,
    stream_id: Option<u64>,
    event: Option<Arc<crate::Event>>,
) {
    if let Some(alloc) = keep_alive.downcast_ref::<ManagedAlloc>() {
        alloc.set_fence(stream_id, event);
    }
}

pub(crate) struct DeviceBufferInner<T> {
    alloc: Arc<ManagedAlloc>,
    len: usize,
    _marker: std::marker::PhantomData<T>,
}

impl<T> DeviceBufferInner<T> {
    fn bytes(len: usize) -> Result<usize> {
        len.checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| DeviceError::Memcpy("device alloc size overflows".into()))
    }

    fn alloc(ctx: &DeviceContextInner, len: usize, initial: RetireState) -> Result<Self> {
        require_opencl!(_cl);
        let bytes = Self::bytes(len)?;
        let aligned_req = if bytes == 0 {
            0
        } else {
            align_up_ocl(bytes, ctx.sub_align)
        };
        let lease = ctx.alloc_pool.alloc(aligned_req, None)?;
        let (mem, owns_mem) = match ctx.alloc_pool.backend().acquire_mem_for_lease(&lease) {
            Ok(pair) => pair,
            Err(err) => {
                ctx.alloc_pool.free_deferred(lease, None, None);
                return Err(err);
            }
        };
        Ok(Self {
            alloc: Arc::new(ManagedAlloc {
                mem,
                owns_mem,
                shared_host_mapped: false,
                host_ptr: None,
                _keep_alive: None,
                lease: Some(lease),
                pool: Arc::clone(&ctx.alloc_pool),
                ctx: ctx.clone(),
                retire: Mutex::new(initial),
            }),
            len,
            _marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        Self::alloc(ctx, len, RetireState::idle())
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        require_opencl!(cl);
        let buf = Self::alloc(ctx, len, RetireState::idle())?;
        let bytes = Self::bytes(len)?;
        if bytes > 0 {
            if let Some(fill) = cl.clEnqueueFillBuffer {
                let pattern: u8 = 0;
                // SAFETY: `buf.alloc.mem` holds at least `bytes` bytes and
                // OpenCL copies the 1-byte `pattern` synchronously during enqueue.
                unsafe {
                    check(
                        "clEnqueueFillBuffer(zeros)",
                        fill(
                            ctx.copy_h2d.raw,
                            buf.alloc.mem,
                            &pattern as *const u8 as *const c_void,
                            1,
                            0,
                            bytes,
                            0,
                            std::ptr::null(),
                            std::ptr::null_mut(),
                        ),
                    )?;
                }
                RetireState::set_unfenced(&buf.alloc.retire);
            } else {
                let zero = vec![0u8; bytes];
                // SAFETY: `buf.alloc.mem` holds at least `bytes` bytes and `zero`
                // stays live across the blocking write (OpenCL 1.1 fallback).
                unsafe {
                    check(
                        "clEnqueueWriteBuffer(zeros)",
                        (cl.clEnqueueWriteBuffer)(
                            ctx.copy_h2d.raw,
                            buf.alloc.mem,
                            CL_TRUE,
                            0,
                            bytes,
                            zero.as_ptr() as *const c_void,
                            0,
                            std::ptr::null(),
                            std::ptr::null_mut(),
                        ),
                    )?;
                }
            }
        }
        Ok(buf)
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        let buf = Self::alloc(ctx, host.len(), RetireState::idle())?;
        buf.copy_from_host(host)?;
        Ok(buf)
    }

    /// Wrap a caller-owned host slice using `CL_MEM_USE_HOST_PTR` for zero-copy APU execution.
    ///
    /// # Safety
    ///
    /// `host` must remain live and unmodified/unrelocated for the lifetime of this
    /// buffer and all queued OpenCL commands referencing it.
    pub(crate) unsafe fn from_shared_host_unchecked(
        ctx: &DeviceContextInner,
        host: &[T],
        _mapped_dev_ptr: Option<u64>,
        keep_alive: Option<BufferKeepAlive>,
    ) -> Result<Self> {
        require_opencl!(cl);
        if host.is_empty() {
            return Self::alloc(ctx, 0, RetireState::idle());
        }
        if !ctx.device.shared_memory.supports_zero_copy_shared_memory()
            && !ctx.device.shared_memory.unified_addressing
        {
            return Err(DeviceError::Driver(
                "zero-copy shared host mapping is only supported on integrated shared-memory OpenCL devices"
                    .into(),
            ));
        }
        let bytes = Self::bytes(host.len())?;
        let mut err: cl_int = 0;
        // SAFETY: `host` covers `bytes` bytes and caller guarantees liveness until retire.
        let mem = unsafe {
            (cl.clCreateBuffer)(
                ctx.device.context,
                CL_MEM_READ_WRITE | CL_MEM_USE_HOST_PTR,
                bytes,
                host.as_ptr() as *mut c_void,
                &mut err,
            )
        };
        check("clCreateBuffer(CL_MEM_USE_HOST_PTR)", err)?;
        Ok(Self {
            alloc: Arc::new(ManagedAlloc {
                mem,
                owns_mem: true,
                shared_host_mapped: true,
                host_ptr: Some(host.as_ptr() as usize),
                _keep_alive: keep_alive,
                lease: None,
                pool: Arc::clone(&ctx.alloc_pool),
                ctx: ctx.clone(),
                retire: Mutex::new(RetireState::idle()),
            }),
            len: host.len(),
            _marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn new_shared_zeroed(ctx: &DeviceContextInner, len: usize) -> Result<(Self, bool)> {
        if len > 0 && ctx.device.shared_memory.supports_zero_copy_shared_memory() {
            require_opencl!(cl);
            let bytes = Self::bytes(len)?;
            let mut err: cl_int = 0;
            // SAFETY: `ctx.device.context` is live and `err` is a valid out-param.
            let mem = unsafe {
                (cl.clCreateBuffer)(
                    ctx.device.context,
                    CL_MEM_READ_WRITE | CL_MEM_ALLOC_HOST_PTR,
                    bytes,
                    std::ptr::null_mut(),
                    &mut err,
                )
            };
            if err == CL_SUCCESS && !mem.is_null() {
                let mut map_err: cl_int = 0;
                // SAFETY: `mem` owns `bytes` newly allocated bytes; synchronous map zero-fills them.
                let mapped = unsafe {
                    (cl.clEnqueueMapBuffer)(
                        ctx.copy_h2d.raw,
                        mem,
                        CL_TRUE,
                        CL_MAP_WRITE,
                        0,
                        bytes,
                        0,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        &mut map_err,
                    )
                };
                if map_err == CL_SUCCESS && !mapped.is_null() {
                    // SAFETY: `mapped` points to `bytes` exclusively mapped writable bytes.
                    unsafe {
                        std::ptr::write_bytes(mapped.cast::<u8>(), 0, bytes);
                        let _ = (cl.clEnqueueUnmapMemObject)(
                            ctx.copy_h2d.raw,
                            mem,
                            mapped,
                            0,
                            std::ptr::null(),
                            std::ptr::null_mut(),
                        );
                        let _ = (cl.clFinish)(ctx.copy_h2d.raw);
                    }
                    return Ok((
                        Self {
                            alloc: Arc::new(ManagedAlloc {
                                mem,
                                owns_mem: true,
                                shared_host_mapped: true,
                                host_ptr: None,
                                _keep_alive: None,
                                lease: None,
                                pool: Arc::clone(&ctx.alloc_pool),
                                ctx: ctx.clone(),
                                retire: Mutex::new(RetireState::idle()),
                            }),
                            len,
                            _marker: std::marker::PhantomData,
                        },
                        false,
                    ));
                }
                // SAFETY: map failed; release `mem` before falling back to `Self::zeros`.
                unsafe {
                    let _ = (cl.clReleaseMemObject)(mem);
                }
            }
        }
        Ok((Self::zeros(ctx, len)?, true))
    }

    pub(crate) fn is_shared_host_mapped(&self) -> bool {
        self.alloc.shared_host_mapped
    }

    /// Allocate and upload `host` asynchronously onto `upload_stream`, recording
    /// `last_write_event` upon completion.
    ///
    /// # Safety
    ///
    /// `host` must remain allocated and unmodified until `upload_stream` drains;
    /// if non-null, `last_write_event` must point to a valid [`OpenClEventHandle`].
    pub(crate) unsafe fn from_host_async_unchecked(
        ctx: &DeviceContextInner,
        host: &[T],
        upload_stream: cl_command_queue,
        last_write_event: *mut OpenClEventHandle,
    ) -> Result<Self> {
        require_opencl!(cl);
        let buf = Self::alloc(ctx, host.len(), RetireState::idle())?;
        if !host.is_empty() {
            let bytes = Self::bytes(host.len())?;
            // SAFETY: caller guarantees `host` outlives the non-blocking write.
            unsafe {
                check(
                    "clEnqueueWriteBuffer(async)",
                    (cl.clEnqueueWriteBuffer)(
                        upload_stream,
                        buf.alloc.mem,
                        CL_FALSE,
                        0,
                        bytes,
                        host.as_ptr() as *const c_void,
                        0,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                    ),
                )?;
            }
            RetireState::set_unfenced(&buf.alloc.retire);
        }
        if !last_write_event.is_null() {
            // SAFETY: caller guarantees `last_write_event` is valid on `upload_stream`.
            unsafe {
                <OpenClBackend as crate::backend_api::DriverApi>::event_record(
                    last_write_event,
                    upload_stream,
                )?;
            }
        }
        Ok(buf)
    }

    pub(crate) fn copy_from_host(&self, host: &[T]) -> Result<()> {
        if host.len() != self.len {
            return Err(DeviceError::Memcpy(format!(
                "copy_from_host length mismatch: host.len()={}, buffer.len()={}",
                host.len(),
                self.len
            )));
        }
        if host.is_empty() {
            return Ok(());
        }
        require_opencl!(cl);
        let bytes = Self::bytes(self.len)?;
        // SAFETY: `host` stays borrowed for the blocking `CL_TRUE` write.
        unsafe {
            check(
                "clEnqueueWriteBuffer",
                (cl.clEnqueueWriteBuffer)(
                    self.alloc.ctx.copy_h2d.raw,
                    self.alloc.mem,
                    CL_TRUE,
                    0,
                    bytes,
                    host.as_ptr() as *const c_void,
                    0,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                ),
            )?;
        }
        Ok(())
    }

    pub(crate) fn to_host(&self, dst: &mut [T], wait_event: Option<*mut OpenClEventHandle>) -> Result<()> {
        if dst.len() != self.len {
            return Err(DeviceError::Memcpy(format!(
                "to_host length mismatch: dst.len()={}, buffer.len()={}",
                dst.len(),
                self.len
            )));
        }
        if self.len == 0 {
            return Ok(());
        }
        require_opencl!(cl);
        let bytes = Self::bytes(self.len)?;
        // SAFETY: `wait_event` comes from the owning buffer's `Arc<Event>`, so its
        // `OpenClEventHandle` outlives this lookup.
        let wait_ev = unsafe { extract_cl_event(wait_event) };
        let wait_slice = wait_ev.map(|e| [e]);
        let (num_wait, wait_ptr) = match &wait_slice {
            Some(s) => (1, s.as_ptr()),
            None => (0, std::ptr::null()),
        };
        if self.alloc.host_ptr == Some(dst.as_mut_ptr() as usize) {
            let mut map_err: cl_int = 0;
            // SAFETY: `self.alloc.mem` aliases `dst`; mapping + unmapping flushes/invalidates
            // CPU/APU cache lines ordered behind `wait_ptr` without copying memory.
            unsafe {
                let mapped = (cl.clEnqueueMapBuffer)(
                    self.alloc.ctx.copy_d2h.raw,
                    self.alloc.mem,
                    CL_TRUE,
                    CL_MAP_READ,
                    0,
                    bytes,
                    num_wait,
                    wait_ptr,
                    std::ptr::null_mut(),
                    &mut map_err,
                );
                check("clEnqueueMapBuffer(shared)", map_err)?;
                let mut unmap_ev: cl_event = std::ptr::null_mut();
                check(
                    "clEnqueueUnmapMemObject(shared)",
                    (cl.clEnqueueUnmapMemObject)(
                        self.alloc.ctx.copy_d2h.raw,
                        self.alloc.mem,
                        mapped,
                        0,
                        std::ptr::null(),
                        &mut unmap_ev,
                    ),
                )?;
                if !unmap_ev.is_null() {
                    let _ = (cl.clWaitForEvents)(1, &unmap_ev);
                    let _ = (cl.clReleaseEvent)(unmap_ev);
                } else {
                    let _ = (cl.clFinish)(self.alloc.ctx.copy_d2h.raw);
                }
            }
            return Ok(());
        }
        // SAFETY: `dst` stays borrowed and uniquely writable for the blocking read.
        unsafe {
            check(
                "clEnqueueReadBuffer",
                (cl.clEnqueueReadBuffer)(
                    self.alloc.ctx.copy_d2h.raw,
                    self.alloc.mem,
                    CL_TRUE,
                    0,
                    bytes,
                    dst.as_mut_ptr() as *mut c_void,
                    num_wait,
                    wait_ptr,
                    std::ptr::null_mut(),
                ),
            )?;
        }
        Ok(())
    }

    pub(crate) fn to_host_async_raw(
        &self,
        dst: &mut [T],
        user_stream: cl_command_queue,
        wait_event: Option<*mut OpenClEventHandle>,
    ) -> Result<()> {
        self.to_host_async_range_raw(dst, 0, user_stream, wait_event)
    }

    pub(crate) fn to_host_async_range_raw(
        &self,
        dst: &mut [T],
        offset: usize,
        user_stream: cl_command_queue,
        wait_event: Option<*mut OpenClEventHandle>,
    ) -> Result<()> {
        let end = offset.checked_add(dst.len()).ok_or_else(|| {
            DeviceError::Memcpy("to_host_async_range: offset + len overflows".into())
        })?;
        if end > self.len {
            return Err(DeviceError::Memcpy(format!(
                "to_host_async_range out of bounds: offset={offset} len={} buffer={}",
                dst.len(),
                self.len
            )));
        }
        if dst.is_empty() {
            return Ok(());
        }
        require_opencl!(cl);
        let byte_offset = Self::bytes(offset)?;
        let bytes = Self::bytes(dst.len())?;
        // SAFETY: `wait_event` comes from the owning buffer's `Arc<Event>`.
        let wait_ev = unsafe { extract_cl_event(wait_event) };
        let wait_slice = wait_ev.map(|e| [e]);
        let (num_wait, wait_ptr) = match &wait_slice {
            Some(s) => (1, s.as_ptr()),
            None => (0, std::ptr::null()),
        };
        // SAFETY: range is bounds-checked above; `dst` remains pinned/live
        // until the caller waits on `user_stream`.
        unsafe {
            check(
                "clEnqueueReadBuffer(async)",
                (cl.clEnqueueReadBuffer)(
                    user_stream,
                    self.alloc.mem,
                    CL_FALSE,
                    byte_offset,
                    bytes,
                    dst.as_mut_ptr() as *mut c_void,
                    num_wait,
                    wait_ptr,
                    std::ptr::null_mut(),
                ),
            )?;
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn bind_to_thread(&self) -> Result<()> {
        Ok(())
    }

    /// Record the stream/event completion fence used when retiring this
    /// allocation to [`OpenClSubAllocator::free_deferred`].
    pub(crate) fn set_retire_with_fence(
        &self,
        stream_id: Option<u64>,
        event: Option<Arc<crate::Event>>,
    ) {
        self.alloc.set_fence(stream_id, event);
    }

    pub(crate) fn add_retire_fence(
        &self,
        stream_id: Option<u64>,
        event: Option<Arc<crate::Event>>,
    ) {
        self.alloc.add_fence(stream_id, event);
    }
}

pub(crate) type BufferKeepAlive = Arc<dyn std::any::Any + Send + Sync + 'static>;

impl<T: Send + Sync + 'static> DeviceBufferInner<T> {
    pub(crate) fn device_ptr_arg(&self) -> (u64, BufferKeepAlive) {
        (
            self.alloc.mem as u64,
            Arc::clone(&self.alloc) as BufferKeepAlive,
        )
    }
}

// ── Pinned Host Buffer ────────────────────────────────────────────────

pub(crate) struct PinnedHostInner<T: Copy> {
    mem: cl_mem,
    ptr: *mut T,
    len: usize,
    ctx: DeviceContextInner,
    _marker: std::marker::PhantomData<T>,
}

// SAFETY: mapped host region is owned uniquely by `PinnedHostInner` and callers
// synchronize DMA before concurrent host access via `as_mut_slice`'s contract.
unsafe impl<T: Copy + Send> Send for PinnedHostInner<T> {}
// SAFETY: as above.
unsafe impl<T: Copy + Sync> Sync for PinnedHostInner<T> {}

impl<T: Copy> PinnedHostInner<T> {
    pub(crate) fn new(ctx: &crate::DeviceContext, len: usize) -> Result<Self> {
        require_opencl!(cl);
        let inner = ctx.inner().clone();
        let bytes = len
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| DeviceError::Memcpy("pinned host alloc size overflows".into()))?;

        let mut err: cl_int = 0;
        // SAFETY: `inner.device.context` is a valid live OpenCL context.
        let mem = unsafe {
            (cl.clCreateBuffer)(
                inner.device.context,
                CL_MEM_READ_WRITE | CL_MEM_ALLOC_HOST_PTR,
                bytes.max(1),
                std::ptr::null_mut(),
                &mut err,
            )
        };
        check("clCreateBuffer(host_ptr)", err)?;

        // SAFETY: `mem` was just created with size `bytes.max(1)` on `inner.device.context`.
        let ptr = unsafe {
            (cl.clEnqueueMapBuffer)(
                inner.copy_h2d.raw,
                mem,
                CL_TRUE,
                CL_MAP_READ | CL_MAP_WRITE,
                0,
                bytes.max(1),
                0,
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut err,
            )
        };
        if err != CL_SUCCESS {
            // SAFETY: release `mem` on map failure.
            unsafe {
                let _ = (cl.clReleaseMemObject)(mem);
            }
            return Err(cl_err("clEnqueueMapBuffer", err));
        }

        Ok(Self {
            mem,
            ptr: ptr as *mut T,
            len,
            ctx: inner,
            _marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Return a mutable slice over the mapped host buffer.
    ///
    /// # Safety
    ///
    /// Caller must ensure no in-flight DMA or concurrent reference aliases the
    /// mapped memory.
    pub(crate) unsafe fn as_mut_slice(&self) -> &mut [T] {
        // SAFETY: `self.ptr` was mapped for at least `self.len` elements of `T`
        // and caller guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl<T: Copy> Drop for PinnedHostInner<T> {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
            // SAFETY: `self.ptr` is still mapped from `self.mem` until `Drop`.
            unsafe {
                let _ = (cl.clEnqueueUnmapMemObject)(
                    self.ctx.copy_h2d.raw,
                    self.mem,
                    self.ptr as *mut c_void,
                    0,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                );
                let _ = (cl.clReleaseMemObject)(self.mem);
            }
        }
    }
}

// ── Backend and Driver Trait Implementations ──────────────────────────

pub(crate) struct OpenClBackend;
pub(crate) type Backend = OpenClBackend;

impl crate::backend_api::BackendApi for OpenClBackend {
    type Context = DeviceContextInner;
    type Module = DeviceModuleInner;
    type Stream = Arc<OpenClStream>;

    fn probe_device(device_ordinal: u32) -> Result<DeviceCaps> {
        probe_device(device_ordinal)
    }

    fn backend_kind() -> crate::BackendKind {
        crate::BackendKind::OpenCl
    }

    fn toolchain_version() -> Result<crate::ToolchainVersion> {
        toolchain_version()
    }
}

impl crate::backend_api::DeviceContextApi for DeviceContextInner {
    fn new(device_ordinal: u32) -> Result<Self> {
        Self::new(device_ordinal)
    }

    fn synchronize(&self) -> Result<()> {
        self.synchronize()
    }

    fn bind_to_thread(&self) -> Result<()> {
        self.bind_to_thread()
    }

    fn record_alloc_event(&self, event: &crate::Event) -> Result<()> {
        self.record_alloc_event(event)
    }

    fn shared_memory_caps(&self) -> SharedMemoryCaps {
        self.device.shared_memory
    }
}

impl crate::backend_api::DeviceModuleApi for DeviceModuleInner {
    type Ctx = DeviceContextInner;
    type Stream = Arc<OpenClStream>;

    fn from_kernel_image(
        ctx: &Self::Ctx,
        image: &crate::KernelImage<'_>,
        module_name: &str,
        kernel_names: &[&'static str],
    ) -> Result<Self> {
        match image {
            crate::KernelImage::OpenClSource(source) => {
                Self::from_opencl_source(ctx, source, module_name, kernel_names)
            }
            crate::KernelImage::Ptx(_) => Err(DeviceError::UnsupportedImage(
                "PTX handed to OpenCL backend; OpenCL compiler accepts OpenCL C source",
            )),
            crate::KernelImage::HipSource { .. } | crate::KernelImage::HipCodeObject(_) => {
                Err(DeviceError::UnsupportedImage(
                    "HIP kernel handed to OpenCL backend",
                ))
            }
        }
    }

    fn optimal_block_size(&self, ctx: &Self::Ctx, kernel: &str) -> Option<u32> {
        self.optimal_block_size(ctx, kernel)
    }

    fn launch_raw_on_stream(
        &self,
        ctx: &Self::Ctx,
        stream: &Self::Stream,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> Result<()> {
        self.launch_raw_on_stream(ctx, stream, kernel, cfg, args)
    }
}

unsafe extern "C" fn host_callback_trampoline(
    _event: cl_event,
    _status: cl_int,
    user_data: *mut c_void,
) {
    unsafe { crate::backend_api::run_boxed_host_callback(user_data) };
}

impl crate::backend_api::DriverApi for OpenClBackend {
    type DeviceHandle = Arc<OpenClDevice>;
    type StreamHandle = Arc<OpenClStream>;
    type RawStream = cl_command_queue;
    type RawEvent = *mut OpenClEventHandle;

    fn device_bind(device: &Self::DeviceHandle) -> Result<()> {
        CURRENT_DEVICE.with(|c| {
            *c.borrow_mut() = Some(Arc::clone(device));
        });
        Ok(())
    }

    fn fork_default_stream(device: &Self::DeviceHandle) -> Result<Self::StreamHandle> {
        require_opencl!(cl);
        let mut err: cl_int = 0;
        // SAFETY: `device.context` and `device.device_id` are valid live handles.
        let q = unsafe {
            (cl.clCreateCommandQueue)(device.context, device.device_id, 0, &mut err)
        };
        check("clCreateCommandQueue", err)?;
        // Order the forked queue after any work already enqueued on `default_queue`.
        if let (Some(marker_fn), Some(barrier_fn)) =
            (cl.clEnqueueMarkerWithWaitList, cl.clEnqueueBarrierWithWaitList)
        {
            let mut ev: cl_event = std::ptr::null_mut();
            // SAFETY: `device.default_queue` and `q` are live queues on the same context.
            let rc = unsafe { marker_fn(device.default_queue, 0, std::ptr::null(), &mut ev) };
            if rc == CL_SUCCESS && !ev.is_null() {
                let b_rc = unsafe { barrier_fn(q, 1, &ev, std::ptr::null_mut()) };
                unsafe {
                    let _ = (cl.clReleaseEvent)(ev);
                }
                if b_rc != CL_SUCCESS {
                    unsafe {
                        let _ = (cl.clReleaseCommandQueue)(q);
                    }
                    return Err(cl_err("clEnqueueBarrierWithWaitList(fork)", b_rc));
                }
            }
        } else {
            // SAFETY: `device.default_queue` is live; OpenCL 1.1 fallback.
            unsafe {
                check("clFinish(default_queue)", (cl.clFinish)(device.default_queue))?;
            }
        }
        let stream = Arc::new(OpenClStream {
            raw: q,
            device: Arc::clone(device),
        });
        let mut guard = device.user_queues.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|w| w.strong_count() > 0);
        guard.push(Arc::downgrade(&stream));
        Ok(stream)
    }

    fn stream_raw(stream: &Self::StreamHandle) -> Self::RawStream {
        stream.raw
    }

    fn stream_token(stream: Self::RawStream) -> usize {
        stream as usize
    }

    fn event_create() -> Result<Self::RawEvent> {
        require_opencl!(cl);
        let dev = CURRENT_DEVICE.with(|c| c.borrow().clone());
        let device = match dev {
            Some(d) => d,
            None => {
                let devs = probe_all_devices(cl)?;
                let first = devs.first().ok_or(DeviceError::NoDriver)?;
                let ctx = DeviceContextInner::new(first.caps.ordinal)?;
                ctx.bind_to_thread()?;
                Arc::clone(&ctx.device)
            }
        };

        // Defer actual `cl_event` allocation until `event_record` overwrites it
        // with a real queue marker (`clEnqueueMarkerWithWaitList`), avoiding a
        // throwaway `clCreateUserEvent` allocation on every event creation.
        let handle = Box::new(OpenClEventHandle {
            cl_event: Mutex::new(None),
            device,
        });
        Ok(Box::into_raw(handle))
    }

    unsafe fn event_record(event: Self::RawEvent, stream: Self::RawStream) -> Result<()> {
        if event.is_null() {
            return Ok(());
        }
        require_opencl!(cl);
        // SAFETY: `DriverApi::event_record` caller guarantees `event` is live.
        let handle = unsafe { &*event };
        let mut new_ev: cl_event = std::ptr::null_mut();
        if let Some(marker_fn) = cl.clEnqueueMarkerWithWaitList {
            // SAFETY: `stream` is a live queue and `new_ev` is a valid out-param.
            check("clEnqueueMarkerWithWaitList", unsafe {
                marker_fn(stream, 0, std::ptr::null(), &mut new_ev)
            })?;
        } else {
            // SAFETY: fallback path when `clEnqueueMarkerWithWaitList` is absent.
            unsafe {
                let _ = (cl.clFlush)(stream);
                let mut err = 0;
                new_ev = (cl.clCreateUserEvent)(handle.device.context, &mut err);
                check("clCreateUserEvent", err)?;
                let _ = (cl.clSetUserEventStatus)(new_ev, CL_COMPLETE);
            }
        }
        let mut lock = handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
        let old_ev = lock.replace(new_ev);
        drop(lock);
        if let Some(old_ev) = old_ev {
            if !old_ev.is_null() {
                // SAFETY: `old_ev` was exclusively held by `handle.cl_event`.
                unsafe {
                    let _ = (cl.clReleaseEvent)(old_ev);
                }
            }
        }
        Ok(())
    }

    unsafe fn event_query(event: Self::RawEvent) -> Result<bool> {
        if event.is_null() {
            return Ok(true);
        }
        require_opencl!(cl);
        // SAFETY: caller guarantees `event` is live.
        let handle = unsafe { &*event };
        let cur_ev = *handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
        let Some(cur_ev) = cur_ev.filter(|e| !e.is_null()) else {
            return Ok(true);
        };
        let mut status: cl_int = 0;
        // SAFETY: `cur_ev` is a valid live `cl_event` and `status` matches `cl_int`.
        let rc = unsafe {
            (cl.clGetEventInfo)(
                cur_ev,
                CL_EVENT_COMMAND_EXECUTION_STATUS,
                std::mem::size_of::<cl_int>(),
                &mut status as *mut _ as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        if rc != CL_SUCCESS {
            return Err(cl_err("clGetEventInfo", rc));
        }
        if status == CL_COMPLETE {
            Ok(true)
        } else if status >= 0 {
            Ok(false)
        } else {
            Err(cl_err("event_query", status))
        }
    }

    unsafe fn event_synchronize(event: Self::RawEvent) -> Result<()> {
        if event.is_null() {
            return Ok(());
        }
        require_opencl!(cl);
        // SAFETY: caller guarantees `event` is live.
        let handle = unsafe { &*event };
        let cur_ev = *handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
        let Some(cur_ev) = cur_ev.filter(|e| !e.is_null()) else {
            return Ok(());
        };
        // SAFETY: `cur_ev` is a live OpenCL event handle.
        check("clWaitForEvents", unsafe { (cl.clWaitForEvents)(1, &cur_ev) })
    }

    unsafe fn event_destroy(event: Self::RawEvent) {
        if event.is_null() {
            return;
        }
        // SAFETY: `event` was created via `Box::into_raw` in `event_create` and
        // is uniquely owned at destroy time.
        let handle = unsafe { Box::from_raw(event) };
        if let Some(cl) = get_opencl() {
            if let Some(ev) = *handle.cl_event.lock().unwrap_or_else(|p| p.into_inner()) {
                if !ev.is_null() {
                    // SAFETY: `ev` was exclusively owned by `handle`.
                    unsafe {
                        let _ = (cl.clReleaseEvent)(ev);
                    }
                }
            }
        }
    }

    unsafe fn stream_wait_event(stream: Self::RawStream, event: Self::RawEvent) -> Result<()> {
        if event.is_null() {
            return Ok(());
        }
        require_opencl!(cl);
        // SAFETY: caller guarantees `event` is live.
        let handle = unsafe { &*event };
        let cur_ev = *handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
        let Some(cur_ev) = cur_ev.filter(|e| !e.is_null()) else {
            return Ok(());
        };
        if let Some(barrier_fn) = cl.clEnqueueBarrierWithWaitList {
            // SAFETY: `stream` and `cur_ev` belong to the same OpenCL context.
            check("clEnqueueBarrierWithWaitList", unsafe {
                barrier_fn(stream, 1, &cur_ev, std::ptr::null_mut())
            })?;
        } else {
            // SAFETY: fallback synchronization when barrier API is unavailable.
            check("clWaitForEvents", unsafe { (cl.clWaitForEvents)(1, &cur_ev) })?;
        }
        Ok(())
    }

    unsafe fn stream_synchronize(stream: Self::RawStream) -> Result<()> {
        require_opencl!(cl);
        // SAFETY: caller guarantees `stream` is a live command queue.
        check("clFinish", unsafe { (cl.clFinish)(stream) })
    }

    unsafe fn launch_host_func(stream: Self::RawStream, f: Box<dyn FnOnce() + Send>) -> Result<()> {
        require_opencl!(cl);
        let Some(set_cb) = cl.clSetEventCallback else {
            return Err(DeviceError::Driver(
                "clSetEventCallback is not supported on this OpenCL runtime".into(),
            ));
        };
        let mut ev: cl_event = std::ptr::null_mut();
        if let Some(marker_fn) = cl.clEnqueueMarkerWithWaitList {
            // SAFETY: `stream` is live and `ev` is a valid out-param.
            check("clEnqueueMarkerWithWaitList", unsafe {
                marker_fn(stream, 0, std::ptr::null(), &mut ev)
            })?;
        } else {
            return Err(DeviceError::Driver(
                "clEnqueueMarkerWithWaitList is not supported on this OpenCL runtime".into(),
            ));
        }

        let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(f);
        let user_data = Box::into_raw(boxed) as *mut c_void;
        // SAFETY: `ev` is live and `host_callback_trampoline` consumes `user_data` once.
        let rc = unsafe { set_cb(ev, CL_COMPLETE, host_callback_trampoline, user_data) };
        if rc != CL_SUCCESS {
            // SAFETY: callback registration failed so we reclaim `user_data` and `ev`.
            unsafe {
                drop(Box::from_raw(user_data as *mut Box<dyn FnOnce() + Send>));
                let _ = (cl.clReleaseEvent)(ev);
            }
            return Err(cl_err("clSetEventCallback", rc));
        }
        // Release event handle so it frees once completed.
        // SAFETY: OpenCL retains an internal ref until the callback finishes.
        unsafe {
            let _ = (cl.clReleaseEvent)(ev);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
