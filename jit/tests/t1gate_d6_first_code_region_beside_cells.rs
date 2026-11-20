// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! t1gate D-6: a code buffer (the default, non-arena path) lands beside the cell
//! chunk, so a heap reservation made between "cells" and "first compile" cannot
//! push the code out of `disp32` reach of the gate cells. One test per binary
//! (the cell chunk is process-wide).
#![cfg(all(
    target_arch = "x86_64",
    any(target_os = "linux", target_os = "windows")
))]

use cratonvm_jit::platform::alloc_code_adjacent_cell;
use cratonvm_jit::ExecutableBuffer;

/// Stand-in for the heap: 6 GiB of address space, reserved but unusable.
#[cfg(target_os = "linux")]
fn reserve_heap_standin() {
    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    }
    // PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            6 << 30,
            0,
            0x02 | 0x20 | 0x4000,
            -1,
            0,
        )
    };
    assert_ne!(p as isize, -1, "reservation");
}

#[cfg(target_os = "windows")]
fn reserve_heap_standin() {
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, ty: u32, protect: u32) -> *mut u8;
    }
    // MEM_RESERVE, PAGE_NOACCESS
    let p = unsafe { VirtualAlloc(std::ptr::null_mut(), 6 << 30, 0x2000, 0x01) };
    assert!(!p.is_null(), "reservation");
}

#[test]
fn first_code_region_is_in_disp32_reach_of_the_cells_under_a_big_heap_reservation() {
    let cell = alloc_code_adjacent_cell().expect("cell") as usize;
    reserve_heap_standin();
    let buf = ExecutableBuffer::new(4096).expect("buffer");
    let dist = (buf.as_ptr() as usize).abs_diff(cell);
    assert!(
        dist < (1 << 30) + (1 << 28),
        "code is {dist:#x} bytes from the cells"
    );
}
