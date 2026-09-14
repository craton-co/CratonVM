// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! GC-stable side-table keys for synthetic-JDK collection overlays.
//!
//! LinkedList / LinkedHashMap / TreeMap / TreeSet maintain per-object
//! state in process-global side-tables (see `ll_overlay`, `lhm_overlay`,
//! `tm_array_table`, `ts_array_table`). Originally those tables were
//! keyed by `this.as_ptr() as usize` — a raw heap address that a moving
//! GC can invalidate the instant it relocates an object, silently
//! losing the overlay's content (size, bucket table, head/tail pointers).
//!
//! The fix is to key by `ctx.identity_hash_code(this)` instead. The
//! object header carries a stable identity-hash word that the GC copies
//! along with the object during relocation, so the same `i32` resolves
//! to the same overlay entry pre- and post-move.
//!
//! `obj_key` is the single helper every overlay key site funnels
//! through. Returning `usize` (not `i32`) so it slots into the existing
//! `HashMap<usize, _>` table types without churn at every key call.
//!
//! Identity-hash seeding: `NativeContext::identity_hash_code` seeds the
//! header word on first call (per the GC heap's invariant — see
//! `gc::heap::identity_hash_code`). Collection `<init>` natives call
//! into `obj_key` immediately, so by the time bytecode can observe the
//! overlay it already has a stable key.

use cratonvm_native_api::NativeContext;
use cratonvm_types::ObjectRef;

/// GC-stable key for a side-table entry. Computes `identity_hash_code`
/// via the NativeContext, which (a) seeds the object header's hash
/// word on first invocation and (b) returns the same value across GC
/// moves thereafter.
///
/// The cast to `usize` widens an `i32` while preserving its bit
/// pattern (via the intermediate `u32`) so two objects with distinct
/// identity hashes never collide via sign-extension.
///
/// Exposed at `pub` (under `#[doc(hidden)]`) for the GC-relocation
/// integration harness in `tests/gc_relocation_harness.rs`, which
/// asserts the key is stable across a simulated move.
#[doc(hidden)]
#[inline]
pub fn obj_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    ctx.identity_hash_code(this) as u32 as usize
}

/// Seed the identity-hash word for `this`. Called from collection
/// `<init>` natives so the overlay's first `obj_key` lookup uses a
/// hash that will survive any subsequent GC relocation. Discarding
/// the return value is intentional — we only care about the
/// side-effect of priming the header.
#[doc(hidden)]
#[inline]
pub fn seed(ctx: &dyn NativeContext, this: ObjectRef) {
    let _ = ctx.identity_hash_code(this);
}
