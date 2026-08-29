// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Runtime execution engine.
//!
//! Contains the bytecode interpreter, call stack, operand stack, frame,
//! and exception creation utilities.

mod call_stack;
pub mod ec_watch;
pub mod env_cache;
pub mod exceptions;
pub mod frame;
pub mod interpreter;
pub mod invokedynamic;
pub mod local_liveness;
pub mod memwatch;
pub mod native_oom;
pub mod signals;
pub mod value_stack;

pub mod alloc_fastpath;
pub mod build_tool_compat;
pub mod container;
pub mod crash_handler;
pub mod deopt_materialize;
pub mod diagnostics;
pub mod fx_collections;
pub mod gc_integration;
pub mod heartbeat_watch;
pub mod hprof;
pub mod jdk_layout;
pub mod jit_integration;
pub mod jvmti;
pub mod lambda_proxy;
pub mod lock_order;
pub mod lockfree_resolve;
// C2 review P0 — the one method/field resolution API. Every new resolution
// site goes through `resolve::MemberResolver`; `resolve::guard` is the
// repository check that rejects new direct metadata-table bypasses.
pub mod resolve;
pub mod serialization;
pub mod serviceability;
pub mod shared_secrets;
pub mod soak_test;
pub mod stwhang_watch;
pub mod tck;
pub mod threading_integration;
pub mod unified_logging;
pub mod unsafe_helpers;
pub mod varhandle;
pub mod vtable;
// WP2.6 — `lang_reflect_constructor` hosts the spec-classification
// helpers used by the `Constructor.newInstance` edge-case tests. The
// native still lives in `native-builtins/src/lang_class.rs`; this
// module just exposes the classification logic.
pub mod agent_loader;
pub mod instrument;
pub mod lang_reflect_constructor;
pub mod proxy;
pub(crate) mod redefine_state;
pub mod stackwalker;

// Part C of the GPU offload plan — primitive-array marshalling between the
// JVM heap and CUDA device memory. Strictly gated behind the `gpu-offload`
// Cargo feature so the default VM build is byte-identical to before the
// feature was introduced. Public items in this module are only reachable
// when the feature is on.
#[cfg(feature = "gpu-offload")]
pub mod gpu_marshal;

#[cfg(feature = "gpu-offload")]
pub mod gpu_residency;

/// Built-in device kernels shipped as PTX, for work the bytecode
/// lowering cannot express (see the module docs).
#[cfg(feature = "gpu-offload")]
pub mod kernels;

// Part E of the GPU offload plan — analyzer-cache, PTX module store, and
// dispatcher hook for the interpreter's `execute_invokestatic`. Strictly
// gated behind the `gpu-offload` feature.
#[cfg(feature = "gpu-offload")]
pub mod offload;

// GPU offload follow-up item 2 (fixed-suite-bugs/gpu-offload-followups-20260711.md):
// deny JIT/OSR admission of caller methods that invoke an offload-eligible
// static target, so a JIT-compiled caller can't silently stop offloading.
// Strictly gated behind the `gpu-offload` feature, same as the rest of the
// GPU offload plan.
#[cfg(feature = "gpu-offload")]
pub mod offload_jit_gate;

pub use call_stack::CallStack;
pub use value_stack::ValueStack;
