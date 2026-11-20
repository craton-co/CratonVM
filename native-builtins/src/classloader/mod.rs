// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! ClassLoader hierarchy, URLClassLoader, MethodHandles.Lookup, ProtectionDomain,
//! and CodeSource native method implementations.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::obj_arg;
use crate::service_loader::impl_jars_load_class;
use crate::util_concurrent_ext::try_alloc_concurrent_synthetic;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry, NestedJarRowEvent};
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::error::RuntimeError;
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use cratonvm_types::{ObjectRef, Value};
#[cfg(test)]
mod i143_l5_defining_lock_tests;
#[cfg(test)]
mod i81_loader_kinds_tests;

#[cfg(test)]
mod i120_pd_url_memo_tests;

#[cfg(test)]
mod i83_loader_namespace_index_tests;

#[cfg(test)]
mod i72_supertype_links_tests;
/// zt t27/lg1: Modified UTF-8 names decode exactly -- `C0 80` is NUL, a
/// supplementary character's surrogate pair (U+1F600 = D83D DE00 = `ED A0 BD
/// ED B8 80`) is one char; a lone surrogate or a four-byte form is refused.
#[cfg(test)]
mod zt27lg1_this_class_mutf8_tests;
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod classloader_tests;

/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 20): the closed-`URLClassLoader` set and the single-argument
/// `loadClass` re-entry guard tell two live loaders with one identity hash
/// apart. Mock addresses 4 GiB apart share the mock identity hash (the address
/// truncated to `i32`); never dereferenced.
#[cfg(test)]
mod w29e_loader_collider_tests;

#[cfg(test)]
mod r13w7_predicates_proxy_name_tests;

#[cfg(test)]
mod i64_jar_loader_bytes_tests;

#[cfg(test)]
mod i63_nested_jar_bytes_tests;

#[cfg(test)]
mod i54_nested_jar_walk_tests;

#[cfg(test)]
mod i65_walk_index_per_vm_tests;

#[cfg(test)]
mod i66_shared_outer_tests;

#[cfg(test)]
mod i68_refusal_stamp_tests;

#[cfg(test)]
mod i69_row_event_tests;

#[cfg(test)]
mod i70_jar_loader_class_path_archive_tests;

#[cfg(test)]
mod i71_jar_loader_class_path_direct_tests;

/// zs s9/nd8 (`zgc-zs9rv6-wave-s8-review-residuals-FIXED-20261019.md` item 5): the
/// `catch_unwind` panic arms of `cl_define_class_basic` and
/// `define_class_via_full` release every pin they took (the reservation sits
/// above them), as their `Err(msg)` arms do. Before, both returned with their
/// pins still on the stack (`safe_native_call_impl` cut it back later).
#[cfg(test)]
mod zs9nd8_define_panic_pin_tests;

/// zs s10/nd9 (`zgc-zs9rv6-wave-s8-review-residuals-FIXED-20261019.md` item 4):
/// spent-ladder rows for the `--compatible` definers of this file that had
/// none -- the synthetic `Lookup.defineClass` / `defineHiddenClass` bodies
/// (`lk_define_class`, `lk_define_hidden_class`: registered first, then
/// overwritten by `lookup_define`'s, so reachable only here) and the
/// `URLClassLoader` local define (`ucl_try_define_local_class`). Each row
/// (`test_utils::spent_define_row`): every mirror reservation refused and the
/// reclaim ladder empty -> `OutOfMemoryError`, `define_class_full` never
/// reached, no pin left; then with room the same body defines. Kill control:
/// `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER=0` defines first.
#[cfg(test)]
mod zs10nd9_define_rows;

#[cfg(test)]
mod gci_i4jd_image_app_module_loader_tests;

/// gci i18/vp: the `Resource.getBytes()[B` intrinsic's decline into the
/// JDK's body is a transparent tail call (`continuation_pin::tail_call`), so
/// a virtual thread whose read of a remote class-path entry parks unmounts,
/// as on HotSpot, instead of being pinned by the intrinsic's native frame.
#[cfg(test)]
mod gci_i18vp_resource_tail_call_tests;

#[cfg(test)]
mod i100_l3_resource_name_tests;

/// JIT round 21 wave 10, lane reflect: `lk_class_relation` against JDK 25's
/// `Lookup.in` (`isSamePackage` / `isSamePackageMember`), for the shapes the
/// old internal-name split got wrong (page
/// `r21-review-w68-native-findings-20261019.md` item 1).
#[cfg(test)]
mod r21_lookup_in_relation_tests;

/// zt t2/nd: [`MemoRows`]' position memo answers what a scan of the rows
/// answers, through every writer shape (append, in-place value change, a
/// shorter `retain`, a same-length rewrite after a generation bump, a clear).
#[cfg(test)]
mod zt2nd_memo_rows_tests;

/// zt t3/x2: the reconcile's rewrite drops the [`MemoRows`] memo under the
/// table's lock (`retain_rows_mut`). A lookup between the generation bump and
/// the rewrite rebuilds the memo at the new generation from the old rows; a
/// same-length remap of a non-last row then passes every staleness check.
#[cfg(test)]
mod zt3x2_memo_rows_reconcile_window_tests;

/// zt t6/nd4: the `classes` presize ahead of the define
/// ([`presize_loader_classes`], zt1nd page item 1).
#[cfg(test)]
mod zt6nd4_addclass_presize_tests;

/// zt t7/cl: the `ClassLoader.addClass` upcall of the real-JDK define doors
/// ([`add_defined_class_to_loader`], t6/nd4 finding 14) and their ZGC presize
/// ([`presize_for_add_class`]).
#[cfg(test)]
mod zt7cl_add_class_tests;

#[cfg(test)]
mod zt16df6_bind_after_add_class_tests;

#[cfg(test)]
mod zt18df7_unbound_define_owner_tests;

mod closed_url_classloader_ids;
#[allow(unused_imports)]
pub use closed_url_classloader_ids::*;
#[allow(unused_imports)]
use closed_url_classloader_ids::*;
mod load_class_override_memo_key;
#[allow(unused_imports)]
pub use load_class_override_memo_key::*;
#[allow(unused_imports)]
use load_class_override_memo_key::*;
mod adopt_add_class_orphan_by_id;
#[allow(unused_imports)]
pub use adopt_add_class_orphan_by_id::*;
#[allow(unused_imports)]
use adopt_add_class_orphan_by_id::*;
mod builtin_classloader_find_resource_as;
#[allow(unused_imports)]
pub use builtin_classloader_find_resource_as::*;
#[allow(unused_imports)]
use builtin_classloader_find_resource_as::*;
mod ucl_try_define_local_class;
#[allow(unused_imports)]
pub use ucl_try_define_local_class::*;
#[allow(unused_imports)]
use ucl_try_define_local_class::*;
