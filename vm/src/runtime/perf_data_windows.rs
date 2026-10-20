// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//! Windows jvmstat uses both a backing file and hsperfdata_<user>_<pid>
//! file mapping. File ownership and delete-on-close belong to this VM lease.
use std::{ffi::c_void, path::PathBuf, ptr};
type Handle = *mut c_void;
#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[link(name = "kernel32")]
extern "system" {
    fn CreateDirectoryW(path: *const u16, security: *const SecurityAttributes) -> i32;
    fn GetFileAttributesW(path: *const u16) -> u32;
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        security: *const SecurityAttributes,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn WriteFile(
        file: Handle,
        data: *const u8,
        size: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn FlushFileBuffers(file: Handle) -> i32;
    fn CreateFileMappingW(
        file: Handle,
        security: *const SecurityAttributes,
        protection: u32,
        high: u32,
        low: u32,
        name: *const u16,
    ) -> Handle;
    // `isize`, as every other `CloseHandle` declaration in this crate has it
    // (`xt_root_scan.rs`, `stwhang_watch.rs`, `heartbeat_watch.rs`,
    // `jvm_thread.rs`): `clashing_extern_declarations` is `deny` workspace-wide.
    fn CloseHandle(handle: isize) -> i32;
    fn MapViewOfFile(
        mapping: Handle,
        access: u32,
        offset_high: u32,
        offset_low: u32,
        bytes: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn GetLastError() -> u32;
    fn GetCurrentProcess() -> Handle;
    fn LocalFree(memory: Handle) -> Handle;
}
#[link(name = "advapi32")]
extern "system" {
    fn GetUserNameW(name: *mut u16, size: *mut u32) -> i32;
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        kind: u32,
        data: *mut c_void,
        size: u32,
        needed: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, text: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
unsafe fn wide_string(value: *const u16) -> String {
    let mut size = 0;
    while *value.add(size) != 0 {
        size += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(value, size))
}
#[derive(Debug)]
struct OwnedHandle(usize);
impl OwnedHandle {
    fn raw(&self) -> Handle {
        self.0 as Handle
    }
}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.raw() as isize);
        }
    }
}
struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
#[derive(Debug)]
pub(super) struct Mapping {
    /// A writable view of the whole mapping (0 when none could be made),
    /// through which the live counters are updated (interpreter round i4
    /// wave 86, lane L1): a reader maps the same named mapping, so the views
    /// are coherent, which `WriteFile` to the backing file would not be.
    view: usize,
    mapping: OwnedHandle,
    file: OwnedHandle,
}
impl Mapping {
    /// The writable view's address, if there is one.
    pub(super) fn view_base(&self) -> Option<usize> {
        (self.view != 0).then_some(self.view)
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        if self.view != 0 {
            // SAFETY: this value's own view, unmapped once, before the
            // mapping and file handles close (the fields drop after this).
            unsafe {
                UnmapViewOfFile(self.view as *const c_void);
            }
        }
    }
}
// Field order closes the mapping before the backing file, which is removed by
// the OS even after abrupt process termination. No process-wide owner exists.
fn security_descriptor() -> Option<LocalMemory> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), 8, &mut token) == 0 {
            return None;
        }
        let token = OwnedHandle(token as usize);
        let mut needed = 0;
        GetTokenInformation(token.raw(), 1, ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return None;
        }
        let mut buffer = vec![
            0usize;
            (needed as usize + std::mem::size_of::<usize>() - 1)
                / std::mem::size_of::<usize>()
        ];
        if GetTokenInformation(
            token.raw(),
            1,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return None;
        }
        // TOKEN_USER starts with SID_AND_ATTRIBUTES, whose first member is SID*.
        let sid = *(buffer.as_ptr() as *const *const c_void);
        let mut sid_text = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut sid_text) == 0 {
            return None;
        }
        let allocated = LocalMemory(sid_text.cast());
        let sddl = wide(&format!(
            "D:P(A;;FA;;;SY)(A;;FA;;;{})",
            wide_string(sid_text)
        ));
        drop(allocated);
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return None;
        }
        Some(LocalMemory(descriptor))
    }
}
fn user_name() -> Option<String> {
    let mut buffer = [0u16; 257];
    let mut size = buffer.len() as u32;
    if unsafe { GetUserNameW(buffer.as_mut_ptr(), &mut size) } == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(
        &buffer[..size.saturating_sub(1) as usize],
    ))
}
pub(super) fn publish(pid: u32, bytes: &[u8]) -> Option<(PathBuf, Mapping)> {
    let user = user_name()?;
    if user.contains(['/', '\\', ':', '\0']) {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("hsperfdata_{user}"));
    let descriptor = security_descriptor()?;
    let security = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: descriptor.0,
        inherit: 0,
    };
    let dir_text = wide(dir.to_str()?);
    unsafe {
        CreateDirectoryW(dir_text.as_ptr(), &security);
    }
    let attrs = unsafe { GetFileAttributesW(dir_text.as_ptr()) };
    // Require a directory, refuse all reparse-point redirection.
    if attrs == u32::MAX || attrs & 0x10 == 0 || attrs & 0x400 != 0 {
        return None;
    }
    // The numeric backing file is created delete-on-close, as in HotSpot.
    // The named mapping is exposed only after its complete contents are written.
    let path = dir.join(pid.to_string());
    let path_text = wide(path.to_str()?);
    let name = wide(&format!("hsperfdata_{user}_{pid}"));
    unsafe {
        let raw = CreateFileW(
            path_text.as_ptr(),
            0xc0000000,
            5,
            &security,
            1,
            0x04000080,
            ptr::null_mut(),
        );
        if raw as isize == -1 {
            return None;
        }
        let file = OwnedHandle(raw as usize);
        let mut written = 0;
        if WriteFile(
            file.raw(),
            bytes.as_ptr(),
            u32::try_from(bytes.len()).ok()?,
            &mut written,
            ptr::null_mut(),
        ) == 0
            || written as usize != bytes.len()
            || FlushFileBuffers(file.raw()) == 0
        {
            return None;
        }
        let raw = CreateFileMappingW(
            file.raw(),
            &security,
            4,
            0,
            u32::try_from(bytes.len()).ok()?,
            name.as_ptr(),
        );
        if raw.is_null() {
            return None;
        }
        let existed = GetLastError() == 183;
        let mapping = OwnedHandle(raw as usize);
        // Another VM already owns the name: leave its data and lifetime intact.
        if existed {
            return None;
        }
        // FILE_MAP_WRITE (2), the whole mapping.
        let view = MapViewOfFile(mapping.raw(), 2, 0, 0, bytes.len()) as usize; // Cast: the view's address
        Some((
            path,
            Mapping {
                view,
                mapping,
                file,
            },
        ))
    }
}
