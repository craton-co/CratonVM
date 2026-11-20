// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! invokedynamic support: StringConcatFactory and LambdaMetafactory.
//!
//! Handles the two most common bootstrap methods produced by javac:
//!
//! 1. **StringConcatFactory.makeConcatWithConstants** (Java 9+): compiles `"foo" + bar`
//!    to invokedynamic with a recipe string. `\u0001` = argument placeholder.
//!
//! 2. **LambdaMetafactory.metafactory** (Java 8+): compiles lambdas and method
//!    references to invokedynamic, producing lightweight proxy objects that implement
//!    a functional interface and delegate to the implementation method.

use std::sync::Arc;

use cratonvm_reader::attribute::BootstrapMethod;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

// `NativeContext` traits brought into scope for the `NativeContextImpl` calls
// of the generic-invokedynamic bootstrap path. (A `Class` static argument's
// mirror no longer goes through the context: gc-common w9-c routes it through
// `constants::class_mirror_or_oom`, which collects instead of aborting.)
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

use crate::classloading::resolution::{
    LambdaCallSite, MethodHandle, MethodHandleKind, RecordAccessorGetter, RecordMethodKind,
    ResolvedCallSite, SwitchLabel,
};
use crate::classloading::ClassId;
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::runtime::frame::Frame;
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::{
    create_java_string, create_java_string_uninterned, read_java_string, read_java_string_units,
    NativeContextImpl, SharedVm,
};

mod concat_latin1;

/// Copy a fresh table row this thread just read. A shape that carries a heap
/// reference is not copied.
#[cfg(test)]
fn fill_thread_instruction_site(
    ident: &InstructionIdent<'_>,
    gate: crate::classloading::resolution::RedefineGate,
    link: &GenericIndyLink,
) {
    fill_thread_instruction_site_with(ident, gate, link, false);
}

#[cfg(test)]
mod gcf_f4o_lambda_proxy_tlab_tests;
/// [`value_to_string_checked`] for the unit tests, which have no thread and so
/// never reach a `toString()` that could throw.
#[cfg(test)]
fn value_to_string(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> String {
    value_to_string_checked(shared, thread, value, type_char)
        .unwrap_or_else(|_| "<threw>".to_string())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod w9c_lambda_proxy_alloc_tests;

#[cfg(test)]
mod i16_l1_indy_site_tests;

#[cfg(test)]
mod i20_l4_bridge_tests;

#[cfg(test)]
mod nio21w10_l3_constant_lambda_tests;

/// nio21 wave 11 L4: a linked generic `invokedynamic` instruction is published
/// whatever the table's size (JVMS §5.4.3.6 links it once); only the kill
/// switch `CRATONVM_RESOLUTION_LINKED_PERMANENT=0` keeps the old refusal at
/// [`GENERIC_INDY_SITE_CAP`]. Page
/// `r21-lambda-call-site-eviction-rebootstraps-a-linked-site-FIXED-20261009.md`.
#[cfg(test)]
mod nio21w11_l4_linked_site_permanence_tests;

/// gci i4/wr (`gci-i3wr-weak-tables-and-epoch-review-findings` item 2): the
/// JIT indy direct-leaf memo keys its raw addresses on the address-memo epoch
/// at both ends (the check and the fill), never on the collection count.
#[cfg(test)]
mod gci_i4wr_indy_memo_epoch_tests;

#[cfg(test)]
mod gci_i4jd_lambda_site_recheck_tests;

#[cfg(test)]
mod gci_i33sc_indy_string_static_arg_tests;

/// JIT round 21 wave 1 (lane indy, `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`):
/// the retarget arm / miss count of a mutable guarded site.
#[cfg(test)]
mod r21_indy_retarget_tests;

#[cfg(test)]
mod jr21_indylate_instruction_linked_tests;

#[cfg(test)]
mod j27_indytrap_cold_concat_tests;

mod dbg_indy_all;
#[allow(unused_imports)]
pub use dbg_indy_all::*;
#[allow(unused_imports)]
use dbg_indy_all::*;
mod execute_invokedynamic_at_uncensused;
#[allow(unused_imports)]
pub use execute_invokedynamic_at_uncensused::*;
#[allow(unused_imports)]
use execute_invokedynamic_at_uncensused::*;
mod object_methods_getter_refusal;
#[allow(unused_imports)]
pub use object_methods_getter_refusal::*;
#[allow(unused_imports)]
use object_methods_getter_refusal::*;
mod descriptor_param_count_and_last_is;
#[allow(unused_imports)]
pub use descriptor_param_count_and_last_is::*;
#[allow(unused_imports)]
use descriptor_param_count_and_last_is::*;
