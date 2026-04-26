//! Runtime execution engine.
//!
//! Contains the bytecode interpreter, call stack, operand stack, frame,
//! and exception creation utilities.

mod call_stack;
pub mod exceptions;
pub mod frame;
pub mod interpreter;
pub mod invokedynamic;
pub mod signals;
pub mod value_stack;

pub mod container;
pub mod crash_handler;
pub mod diagnostics;
pub mod unified_logging;
pub mod gc_integration;
pub mod hprof;
pub mod jit_integration;
pub mod jvmti;
pub mod serviceability;
pub mod soak_test;
pub mod tck;
pub mod threading_integration;
pub mod jdk_layout;
pub mod build_tool_compat;
pub mod vtable;
pub mod fx_collections;
pub mod alloc_fastpath;
pub mod lockfree_resolve;
pub mod lock_order;
pub mod serialization;
pub mod shared_secrets;
pub mod unsafe_helpers;
pub mod varhandle;
pub mod lambda_proxy;
// WP2.6 — `lang_reflect_constructor` hosts the spec-classification
// helpers used by the `Constructor.newInstance` edge-case tests. The
// native still lives in `native-builtins/src/lang_class.rs`; this
// module just exposes the classification logic.
pub mod lang_reflect_constructor;
pub mod instrument;
pub mod agent_loader;
pub mod proxy;
pub mod stackwalker;

pub use call_stack::CallStack;
pub use value_stack::ValueStack;
