// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use cratonvm_jit::platform::{alloc_code_adjacent_cell, code_near_globals_stats};
use cratonvm_jit::ExecutableBuffer;

