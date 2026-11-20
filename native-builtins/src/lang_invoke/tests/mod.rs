// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

mod descriptor_scans_are_not_fooled_by_a;
#[path = "i101_l4_bound_field_lane_tests.rs"]
mod i101_l4_bound_field_lane;
mod i68_l4_invoke_links_and_mint_prelink;
mod mh5_tests;
#[path = "vh_value_adapt_tests.rs"]
mod vh_value_adapt;
