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

use crate::{
    DeviceArch, DeviceCaps, DeviceError, KernelArgs, LaunchConfig, OpenClCaps, Result,
};
use libloading::{Library, Symbol};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
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
pub(crate) const CL_DEVICE_GLOBAL_MEM_SIZE: cl_device_info = 0x101F;
pub(crate) const CL_DEVICE_LOCAL_MEM_SIZE: cl_device_info = 0x1023;

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

unsafe impl Send for OpenClFunctions {}
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
            let warp_size = if vendor.to_lowercase().contains("amd")
                || name.to_lowercase().contains("amd")
                || name.to_lowercase().contains("radeon")
            {
                64
            } else {
                32
            };

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
            };

            let caps = DeviceCaps {
                ordinal,
                name,
                compute_major: opencl_version.0,
                compute_minor: opencl_version.1,
                total_global_mem: global_mem,
                arch: DeviceArch::OpenCl(ocl_caps),
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
}

unsafe impl Send for OpenClDevice {}
unsafe impl Sync for OpenClDevice {}

impl Drop for OpenClDevice {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
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

unsafe impl Send for OpenClStream {}
unsafe impl Sync for OpenClStream {}

impl Drop for OpenClStream {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
            unsafe {
                let _ = (cl.clReleaseCommandQueue)(self.raw);
            }
        }
    }
}

// ── Event Handle ──────────────────────────────────────────────────────

pub struct OpenClEventHandle {
    pub(crate) cl_event: Mutex<cl_event>,
    pub(crate) context: cl_context,
}

unsafe impl Send for OpenClEventHandle {}
unsafe impl Sync for OpenClEventHandle {}

thread_local! {
    static CURRENT_DEVICE: std::cell::RefCell<Option<Arc<OpenClDevice>>> = const { std::cell::RefCell::new(None) };
}

// ── Event Pool ────────────────────────────────────────────────────────

const EVENT_POOL_CAP: usize = 4096;

pub(crate) struct EventPool {
    free: Mutex<Vec<*mut OpenClEventHandle>>,
    device: Arc<OpenClDevice>,
}

unsafe impl Send for EventPool {}
unsafe impl Sync for EventPool {}

impl EventPool {
    pub(crate) fn take(&self) -> Option<*mut OpenClEventHandle> {
        self.free.lock().unwrap_or_else(|p| p.into_inner()).pop()
    }

    pub(crate) fn put(&self, handle: *mut OpenClEventHandle) {
        if handle.is_null() {
            return;
        }
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        if free.len() < EVENT_POOL_CAP {
            free.push(handle);
            return;
        }
        drop(free);
        unsafe {
            <OpenClBackend as crate::backend_api::DriverApi>::event_destroy(handle);
        }
    }
}

impl Drop for EventPool {
    fn drop(&mut self) {
        for handle in self.free.lock().unwrap_or_else(|p| p.into_inner()).drain(..) {
            unsafe {
                <OpenClBackend as crate::backend_api::DriverApi>::event_destroy(handle);
            }
        }
    }
}

// ── Context Inner ─────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct DeviceContextInner {
    device: Arc<OpenClDevice>,
    copy_h2d: Arc<OpenClStream>,
    copy_d2h: Arc<OpenClStream>,
    event_pool: Arc<EventPool>,
}

impl DeviceContextInner {
    pub(crate) fn new(device_ordinal: u32) -> Result<Self> {
        require_opencl!(cl);
        let devs = probe_all_devices(cl)?;
        let target = devs
            .into_iter()
            .find(|d| d.caps.ordinal == device_ordinal)
            .ok_or(DeviceError::NoDriver)?;

        let mut err: cl_int = 0;
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

        let default_queue = unsafe {
            (cl.clCreateCommandQueue)(context, target.device, 0, &mut err)
        };
        if err != CL_SUCCESS {
            unsafe {
                let _ = (cl.clReleaseContext)(context);
            }
            return Err(cl_err("clCreateCommandQueue", err));
        }

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
        });

        let q1 = unsafe { (cl.clCreateCommandQueue)(context, target.device, 0, &mut err) };
        check("clCreateCommandQueue(h2d)", err)?;
        let copy_h2d = Arc::new(OpenClStream {
            raw: q1,
            device: Arc::clone(&device),
        });

        let q2 = unsafe { (cl.clCreateCommandQueue)(context, target.device, 0, &mut err) };
        check("clCreateCommandQueue(d2h)", err)?;
        let copy_d2h = Arc::new(OpenClStream {
            raw: q2,
            device: Arc::clone(&device),
        });

        let event_pool = Arc::new(EventPool {
            free: Mutex::new(Vec::new()),
            device: Arc::clone(&device),
        });

        Ok(Self {
            device,
            copy_h2d,
            copy_d2h,
            event_pool,
        })
    }

    pub(crate) fn record_alloc_event(&self, event: &crate::Event) -> Result<()> {
        self.bind_to_thread()?;
        unsafe {
            <OpenClBackend as crate::backend_api::DriverApi>::event_record(
                event.cu_event_raw(),
                self.copy_h2d.raw,
            )
        }
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        require_opencl!(cl);
        unsafe {
            check("clFinish(default)", (cl.clFinish)(self.device.default_queue))?;
            check("clFinish(copy_h2d)", (cl.clFinish)(self.copy_h2d.raw))?;
            check("clFinish(copy_d2h)", (cl.clFinish)(self.copy_d2h.raw))?;
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
    kernels: HashMap<String, cl_kernel>,
    device: Arc<OpenClDevice>,
}

unsafe impl Send for DeviceModuleInner {}
unsafe impl Sync for DeviceModuleInner {}

impl Drop for DeviceModuleInner {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
            unsafe {
                for (_, k) in self.kernels.drain() {
                    let _ = (cl.clReleaseKernel)(k);
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

        let mut kernels = HashMap::with_capacity(kernel_names.len());
        for &kname in kernel_names {
            let c_kname = CString::new(kname)
                .map_err(|_| DeviceError::Load(format!("kernel name `{kname}` contains NUL")))?;
            let kernel = unsafe {
                (cl.clCreateKernel)(program, c_kname.as_ptr(), &mut err)
            };
            if err != CL_SUCCESS {
                unsafe {
                    for (_, k) in kernels.drain() {
                        let _ = (cl.clReleaseKernel)(k);
                    }
                    let _ = (cl.clReleaseProgram)(program);
                }
                return Err(cl_err(&format!("clCreateKernel({kname})"), err));
            }
            kernels.insert(kname.to_string(), kernel);
        }

        Ok(Self {
            program,
            kernels,
            device: Arc::clone(&ctx.device),
        })
    }

    pub(crate) fn optimal_block_size(&self, _ctx: &DeviceContextInner, kernel: &str) -> Option<u32> {
        let cl = get_opencl()?;
        let k = self.kernels.get(kernel)?;
        let mut size: usize = 0;
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
            .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?;

        for (i, arg) in args.raw.iter().enumerate() {
            let idx = i as cl_uint;
            let rc = match arg {
                crate::KernelArg::DevicePtr { addr, .. } => {
                    let mem = *addr as cl_mem;
                    unsafe {
                        (cl.clSetKernelArg)(
                            *k,
                            idx,
                            std::mem::size_of::<cl_mem>(),
                            &mem as *const _ as *const c_void,
                        )
                    }
                }
                crate::KernelArg::I32(val) => unsafe {
                    (cl.clSetKernelArg)(
                        *k,
                        idx,
                        std::mem::size_of::<i32>(),
                        val as *const _ as *const c_void,
                    )
                },
                crate::KernelArg::I64(val) => unsafe {
                    (cl.clSetKernelArg)(
                        *k,
                        idx,
                        std::mem::size_of::<i64>(),
                        val as *const _ as *const c_void,
                    )
                },
                crate::KernelArg::F32(val) => unsafe {
                    (cl.clSetKernelArg)(
                        *k,
                        idx,
                        std::mem::size_of::<f32>(),
                        val as *const _ as *const c_void,
                    )
                },
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

struct Alloc {
    mem: cl_mem,
    ctx: DeviceContextInner,
}

unsafe impl Send for Alloc {}
unsafe impl Sync for Alloc {}

impl Drop for Alloc {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
            unsafe {
                let _ = (cl.clReleaseMemObject)(self.mem);
            }
        }
    }
}

pub(crate) struct DeviceBufferInner<T> {
    alloc: Arc<Alloc>,
    len: usize,
    _marker: std::marker::PhantomData<T>,
}

impl<T> DeviceBufferInner<T> {
    fn bytes(len: usize) -> Result<usize> {
        len.checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| DeviceError::Memcpy("device alloc size overflows".into()))
    }

    fn alloc(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        require_opencl!(cl);
        let bytes = Self::bytes(len)?;
        let mut err: cl_int = 0;
        let mem = unsafe {
            (cl.clCreateBuffer)(
                ctx.device.context,
                CL_MEM_READ_WRITE,
                bytes.max(1),
                std::ptr::null_mut(),
                &mut err,
            )
        };
        check("clCreateBuffer", err)?;
        cratonvm_types::gpu_event_census::note_alloc_pool_miss();
        Ok(Self {
            alloc: Arc::new(Alloc {
                mem,
                ctx: ctx.clone(),
            }),
            len,
            _marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn uninit(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        Self::alloc(ctx, len)
    }

    pub(crate) fn zeros(ctx: &DeviceContextInner, len: usize) -> Result<Self> {
        require_opencl!(cl);
        let buf = Self::alloc(ctx, len)?;
        let bytes = Self::bytes(len)?;
        let zero = vec![0u8; bytes.max(1)];
        unsafe {
            check(
                "clEnqueueWriteBuffer(zeros)",
                (cl.clEnqueueWriteBuffer)(
                    ctx.copy_h2d.raw,
                    buf.alloc.mem,
                    CL_TRUE,
                    0,
                    bytes.max(1),
                    zero.as_ptr() as *const c_void,
                    0,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                ),
            )?;
        }
        Ok(buf)
    }

    pub(crate) fn from_host(ctx: &DeviceContextInner, host: &[T]) -> Result<Self> {
        let buf = Self::alloc(ctx, host.len())?;
        buf.copy_from_host(host)?;
        Ok(buf)
    }

    pub(crate) unsafe fn from_host_async_unchecked(
        ctx: &DeviceContextInner,
        host: &[T],
        upload_stream: cl_command_queue,
        last_write_event: *mut OpenClEventHandle,
    ) -> Result<Self> {
        require_opencl!(cl);
        let buf = Self::alloc(ctx, host.len())?;
        if !host.is_empty() {
            let bytes = Self::bytes(host.len())?;
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
        if !last_write_event.is_null() {
            <OpenClBackend as crate::backend_api::DriverApi>::event_record(
                last_write_event,
                upload_stream,
            )?;
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
        let wait_ev = wait_event.and_then(|h| {
            if h.is_null() {
                None
            } else {
                let lock = unsafe { (*h).cl_event.lock().unwrap_or_else(|p| p.into_inner()) };
                let ev = *lock;
                if ev.is_null() {
                    None
                } else {
                    Some(ev)
                }
            }
        });
        let wait_slice = wait_ev.map(|e| [e]);
        let (num_wait, wait_ptr) = match &wait_slice {
            Some(s) => (1, s.as_ptr()),
            None => (0, std::ptr::null()),
        };
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
        let wait_ev = wait_event.and_then(|h| {
            if h.is_null() {
                None
            } else {
                let lock = unsafe { (*h).cl_event.lock().unwrap_or_else(|p| p.into_inner()) };
                let ev = *lock;
                if ev.is_null() {
                    None
                } else {
                    Some(ev)
                }
            }
        });
        let wait_slice = wait_ev.map(|e| [e]);
        let (num_wait, wait_ptr) = match &wait_slice {
            Some(s) => (1, s.as_ptr()),
            None => (0, std::ptr::null()),
        };
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

    pub(crate) fn set_retire_to_pool(&self) {}
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

unsafe impl<T: Copy + Send> Send for PinnedHostInner<T> {}
unsafe impl<T: Copy + Sync> Sync for PinnedHostInner<T> {}

impl<T: Copy> PinnedHostInner<T> {
    pub(crate) fn new(ctx: &crate::DeviceContext, len: usize) -> Result<Self> {
        require_opencl!(cl);
        let inner = ctx.inner().clone();
        let bytes = len
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| DeviceError::Memcpy("pinned host alloc size overflows".into()))?;

        let mut err: cl_int = 0;
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

    pub(crate) unsafe fn as_mut_slice(&self) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl<T: Copy> Drop for PinnedHostInner<T> {
    fn drop(&mut self) {
        if let Some(cl) = get_opencl() {
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
    if user_data.is_null() {
        return;
    }
    let boxed = unsafe { Box::from_raw(user_data as *mut Box<dyn FnOnce() + Send>) };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        boxed();
    }));
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
        let q = unsafe {
            (cl.clCreateCommandQueue)(device.context, device.device_id, 0, &mut err)
        };
        check("clCreateCommandQueue", err)?;
        Ok(Arc::new(OpenClStream {
            raw: q,
            device: Arc::clone(device),
        }))
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

        let mut err: cl_int = 0;
        let ev = unsafe { (cl.clCreateUserEvent)(device.context, &mut err) };
        check("clCreateUserEvent", err)?;
        unsafe {
            let _ = (cl.clSetUserEventStatus)(ev, CL_COMPLETE);
        }
        let handle = Box::new(OpenClEventHandle {
            cl_event: Mutex::new(ev),
            context: device.context,
        });
        Ok(Box::into_raw(handle))
    }

    unsafe fn event_record(event: Self::RawEvent, stream: Self::RawStream) -> Result<()> {
        if event.is_null() {
            return Ok(());
        }
        require_opencl!(cl);
        let handle = &*event;
        let mut new_ev: cl_event = std::ptr::null_mut();
        if let Some(marker_fn) = cl.clEnqueueMarkerWithWaitList {
            check(
                "clEnqueueMarkerWithWaitList",
                marker_fn(stream, 0, std::ptr::null(), &mut new_ev),
            )?;
        } else {
            let _ = (cl.clFlush)(stream);
            let mut err = 0;
            new_ev = (cl.clCreateUserEvent)(handle.context, &mut err);
            check("clCreateUserEvent", err)?;
            let _ = (cl.clSetUserEventStatus)(new_ev, CL_COMPLETE);
        }
        let mut lock = handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
        let old_ev = *lock;
        *lock = new_ev;
        if !old_ev.is_null() {
            let _ = (cl.clReleaseEvent)(old_ev);
        }
        Ok(())
    }

    unsafe fn event_query(event: Self::RawEvent) -> Result<bool> {
        if event.is_null() {
            return Ok(true);
        }
        require_opencl!(cl);
        let handle = &*event;
        let cur_ev = {
            let lock = handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
            *lock
        };
        if cur_ev.is_null() {
            return Ok(true);
        }
        let mut status: cl_int = 0;
        let rc = (cl.clGetEventInfo)(
            cur_ev,
            CL_EVENT_COMMAND_EXECUTION_STATUS,
            std::mem::size_of::<cl_int>(),
            &mut status as *mut _ as *mut c_void,
            std::ptr::null_mut(),
        );
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
        let handle = &*event;
        let cur_ev = {
            let lock = handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
            *lock
        };
        if cur_ev.is_null() {
            return Ok(());
        }
        check("clWaitForEvents", (cl.clWaitForEvents)(1, &cur_ev))
    }

    unsafe fn event_destroy(event: Self::RawEvent) {
        if event.is_null() {
            return;
        }
        let handle = Box::from_raw(event);
        if let Some(cl) = get_opencl() {
            let ev = *handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
            if !ev.is_null() {
                let _ = (cl.clReleaseEvent)(ev);
            }
        }
    }

    unsafe fn stream_wait_event(stream: Self::RawStream, event: Self::RawEvent) -> Result<()> {
        if event.is_null() {
            return Ok(());
        }
        require_opencl!(cl);
        let handle = &*event;
        let cur_ev = {
            let lock = handle.cl_event.lock().unwrap_or_else(|p| p.into_inner());
            *lock
        };
        if cur_ev.is_null() {
            return Ok(());
        }
        if let Some(barrier_fn) = cl.clEnqueueBarrierWithWaitList {
            check(
                "clEnqueueBarrierWithWaitList",
                barrier_fn(stream, 1, &cur_ev, std::ptr::null_mut()),
            )?;
        } else {
            check("clWaitForEvents", (cl.clWaitForEvents)(1, &cur_ev))?;
        }
        Ok(())
    }

    unsafe fn stream_synchronize(stream: Self::RawStream) -> Result<()> {
        require_opencl!(cl);
        check("clFinish", (cl.clFinish)(stream))
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
            check(
                "clEnqueueMarkerWithWaitList",
                marker_fn(stream, 0, std::ptr::null(), &mut ev),
            )?;
        } else {
            return Err(DeviceError::Driver(
                "clEnqueueMarkerWithWaitList is not supported on this OpenCL runtime".into(),
            ));
        }

        let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(f);
        let user_data = Box::into_raw(boxed) as *mut c_void;
        let rc = set_cb(ev, CL_COMPLETE, host_callback_trampoline, user_data);
        if rc != CL_SUCCESS {
            drop(Box::from_raw(user_data as *mut Box<dyn FnOnce() + Send>));
            let _ = (cl.clReleaseEvent)(ev);
            return Err(cl_err("clSetEventCallback", rc));
        }
        // Release event handle so it frees once completed
        let _ = (cl.clReleaseEvent)(ev);
        Ok(())
    }
}
