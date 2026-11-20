// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Compiled TLAB allocation must write typed null defaults for reference-only
//! classes. A zeroed legacy Value cell is Int(0), not Object(None).
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

