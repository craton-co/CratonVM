// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The short reference-read barrier check must observe the same live word.

use super::*;

fn emit_guard(address: usize) -> Lowerer<'static> {
    let mut lo = super::r16_codegen_tests::emitter_lowerer();
    lo.read_bounds_addr = address;
    let mut slow = Vec::new();
    lo.emit_read_bounds_live_check(&mut slow);
    lo.buf.emit(&[0xb8, 1, 0, 0, 0, 0xc3]); // live: return 1
    for patch in slow {
        lo.patch_rel32_to_here(patch);
    }
    lo.buf.emit(&[0x31, 0xc0, 0xc3]); // cleared: return 0
    assert!(!lo.buf.overflowed());
    lo.buf.finalize();
    lo
}

#[test]
fn read_bounds_live_encoding_obeys_signed_address_limit() {
    for (address, short) in [
        (0usize, true),
        (0x7fff_fff7, true),
        (0x7fff_fff8, false),
        (usize::MAX, false),
    ] {
        let lo = emit_guard(address);
        let bytes = lo.buf.as_slice();
        if short {
            assert_eq!(&bytes[..4], &[0x48, 0x83, 0x3c, 0x25]);
            assert_eq!(&bytes[4..8], &((address + 8) as i32).to_le_bytes());
            assert_eq!(bytes[8], 0);
        } else {
            assert_eq!(&bytes[..2], &[0x49, 0xbb]);
            assert_eq!(&bytes[2..10], &(address as u64).to_le_bytes());
            assert_eq!(&bytes[10..15], &[0x49, 0x83, 0x7b, 0x08, 0]);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn read_bounds_live_executes_both_address_forms() {
    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    // MAP_32BIT gives the absolute-disp32 arm real writable storage.
    // SAFETY: this requests a fresh anonymous writable mapping; failure is checked before use.
    let low = unsafe { mmap(std::ptr::null_mut(), 4096, 3, 0x22 | 0x40, -1, 0) };
    assert_ne!(low as isize, -1, "low-address mapping failed");
    let mut high = Box::new([0u64; 2]);
    for addr in [low as usize, high.as_mut_ptr() as usize] {
        let lo = emit_guard(addr);
        // SAFETY: both tables own two words; emitted code reads only word 1,
        // returns an integer and has no ABI-visible side effects.
        let call: extern "C" fn() -> u32 = unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        let end = (addr as *mut u64).wrapping_add(1);
        // SAFETY: end names the aligned second word of the live mapping or boxed pair.
        unsafe { end.write(0) };
        assert_eq!(call(), 0);
        // SAFETY: end names the aligned second word of the live mapping or boxed pair.
        unsafe { end.write(0x8000_0000_0000_0000) };
        assert_eq!(call(), 1);
        // SAFETY: end names the aligned second word of the live mapping or boxed pair.
        unsafe { end.write(0) };
        assert_eq!(call(), 0);
    }
    // SAFETY: low is the successful mapping above, released once after the last access.
    assert_eq!(unsafe { munmap(low, 4096) }, 0);
}
