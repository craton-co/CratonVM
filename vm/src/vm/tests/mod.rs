// SPDX-License-Identifier: Apache-2.0
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

pub(crate) mod helpers;
pub(crate) use helpers::*;

mod suite_basics;
mod suite_streams;
mod suite_time_concurrency;
mod suite_io_security;
mod suite_atomics_zip;
mod suite_blocking_ffi;
mod suite_data_streams;
mod suite_classloading_jtreg;
