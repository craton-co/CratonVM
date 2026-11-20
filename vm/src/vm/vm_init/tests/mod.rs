// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Inline unit tests for the VM orchestrator — synthetic-jdk builds only.

use super::*;
use crate::classloading::resolution::MethodHandleKind;
use crate::classloading::ClassId;
use crate::classloading::ClassState;
use crate::config::VmConfig;
use crate::error::{MethodCallFailed, MethodCallResult};
use crate::memory::heap::ArrayElementType;
use crate::native::builtins::register_builtins;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};
use cratonvm_native_api::{
    NativeClassAccess, NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_reader::attribute::LazyAttribute;
use std::sync::Arc;

#[cfg(feature = "synthetic-jdk")]
pub(crate) mod helpers;
#[cfg(feature = "synthetic-jdk")]
pub(crate) use helpers::*;

#[cfg(feature = "synthetic-jdk")]
mod suite_basics;
#[cfg(feature = "synthetic-jdk")]
mod suite_streams;
#[cfg(feature = "synthetic-jdk")]
mod suite_time_concurrency;
#[cfg(feature = "synthetic-jdk")]
mod suite_io_security;
#[cfg(feature = "synthetic-jdk")]
mod suite_atomics_zip;
#[cfg(feature = "synthetic-jdk")]
mod suite_blocking_ffi;
#[cfg(feature = "synthetic-jdk")]
mod suite_data_streams;
#[cfg(feature = "synthetic-jdk")]
mod suite_classloading_jtreg;
mod suite_vm_init;
