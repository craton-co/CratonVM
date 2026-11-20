// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! t1gate H4 (page JDISP, J-1): when the process has already used the 64 static
//! TLS slots, the inline frame-record probe must fall back to the CALL path
//! (displacement 0) instead of reading `gs:[0x1480 + idx * 8]` for an expansion
//! index. One test per binary: the probe is cached for the process.
#![cfg(all(windows, target_arch = "x86_64"))]

#[link(name = "kernel32")]
extern "system" {
    fn TlsAlloc() -> u32;
}

#[test]
fn probe_declines_an_expansion_tls_index_without_faulting() {
    assert!(
        cratonvm_jit::x64::precise_inline_frame_record_enabled(),
        "probe must be live, not skipped"
    );
    for _ in 0..70 {
        assert_ne!(unsafe { TlsAlloc() }, u32::MAX, "TlsAlloc");
    }
    assert_eq!(cratonvm_jit::x64::inline_rbp_tls_disp(), 0);
}
