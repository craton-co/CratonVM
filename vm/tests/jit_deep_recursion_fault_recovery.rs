// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for compiled non-tail self-recursion stack recovery.
//!
//! The historical Spring/Groovy failure was an uncatchable native stack fault in
//! compiled recursion. This fixture keeps the shape small and in-tree: warm a
//! non-tail self-recursive static method until the JIT publishes code for it,
//! then call it deeply inside Java `catch (StackOverflowError)`. Passing proves
//! the compiled self-call guard raises a Java-level exception before the native
//! guard page is hit.

#![allow(clippy::unwrap_used)]

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const FIXTURE: &str = "cratonvm/JitDeepRecursionFaultRecovery";










