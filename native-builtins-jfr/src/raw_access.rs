#![allow(unsafe_code)]
#[cfg(windows)]
pub fn windows_memory_status() -> Option<(u64, u64)> {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct MEMORYSTATUSEX {
        dwLength: u32,
        dwMemoryLoad: u32,
        ullTotalPhys: u64,
        ullAvailPhys: u64,
        ullTotalPageFile: u64,
        ullAvailPageFile: u64,
        ullTotalVirtual: u64,
        ullAvailVirtual: u64,
        ullAvailExtendedVirtual: u64,
    }
    extern "system" {
        fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
    }
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0, ullTotalPhys: 0, ullAvailPhys: 0,
        ullTotalPageFile: 0, ullAvailPageFile: 0, ullTotalVirtual: 0,
        ullAvailVirtual: 0, ullAvailExtendedVirtual: 0,
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        None
    } else {
        Some((status.ullTotalPhys, status.ullTotalPageFile))
    }
}

#[cfg(windows)]
pub fn windows_total_physical_memory_bytes() -> u64 {
    windows_memory_status().map(|(phys, _)| phys).unwrap_or(0)
}
