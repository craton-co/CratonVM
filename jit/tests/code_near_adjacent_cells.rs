// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use cratonvm_jit::platform::{alloc_code_adjacent_cell, code_near_globals_stats};
use cratonvm_jit::ExecutableBuffer;

/// This integration binary owns its process-wide placement counters. Use real
/// mappings and allow an OS refusal to take the ordinary fallback.
#[test]
fn code_placement_uses_the_existing_cells_as_its_anchor() {
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_JIT_CODE_NEAR_GLOBALS", Some("0")),
            ("CRATONVM_JIT_CODE_NEAR_ADJACENT_CELLS", Some("1")),
        ],
        || {
            let cell = alloc_code_adjacent_cell().expect("map the existing cell arena");
            let (placed_before, fallback_before, _) = code_near_globals_stats();
            let buf = ExecutableBuffer::new(64 * 1024).expect("map a code buffer");
            let (placed, fallback, _) = code_near_globals_stats();
            let attempts = placed - placed_before + fallback - fallback_before;
            assert!(attempts <= 1, "a buffer takes at most one fallback attempt");
            if attempts == 0 || placed > placed_before {
                let addr = buf.as_ptr() as usize;
                assert!((addr + 64 * 1024).abs_diff(cell as usize) < 1_500_000_000);
            }
        },
    );
}
