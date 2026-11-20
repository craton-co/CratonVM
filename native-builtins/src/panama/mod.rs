// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Panama FFI native method registrations.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// `LAYOUT_PADDING`, `LAYOUT_SEQUENCE`, `LAYOUT_STRUCT` and `LAYOUT_UNION` are
// NOT imported here any more (F16, 2026-08-13): the group-layout family moved
// wholesale to `phases_late/foreign_ffm.rs`, which does not tag a layout with a
// kind at all, so this file's last uses of the four compound tags are in its
// own `#[cfg(test)]` module — which imports them itself.
use cratonvm_native_api::ffi::{
    self, LAYOUT_ADDRESS, LAYOUT_BOOLEAN, LAYOUT_BYTE, LAYOUT_CHAR, LAYOUT_DOUBLE, LAYOUT_FLOAT,
    LAYOUT_INT, LAYOUT_LONG, LAYOUT_SHORT,
};

// --- ValueLayout: type descriptors for native memory ---
//
// `pe_make_layout` IS A TEST FIXTURE AND NOTHING ELSE (F16, 2026-08-13).
//
// It is the last thing in this crate that builds the `[0]=Int(kind)` layout
// object, and it is `#[cfg(test)]` so that it cannot become a second
// production encoding again. The shipping encoding — the JDK's own
// `[0]=Long(byteSize), [1]=Long(byteAlignment), …` — is minted only by
// `phases_late/foreign_ffm.rs::p67_layout_object` and the four group-layout
// factories beside it. See the banner in `register_pe_value_layout` below.
//
// The kind tag it writes is still MEANINGFUL to the downcall marshaller:
// `panama_libffi::read_layout_kind` reads slot 0 as `Int(kind)` first and
// falls back to resolving the layout's CLASS NAME when slot 0 is a `Long`,
// which is how the shipping carriers are decoded. That fallback is why
// deleting the production rows did not break the FFI path — but it is also a
// reconciliation layer for two encodings that now has only one left to
// reconcile, and it carries a `_ => LAYOUT_LONG` default that is wrong for the
// real JDK's own `ValueLayouts$Of*Impl` class names. Nominated, not this
// lane's file.
#[cfg(test)]
const PE_VALUE_LAYOUT_NAME_SLOT: usize = 2;

#[cfg(test)]
fn pe_make_layout(ctx: &mut dyn NativeContext, kind: i32) -> Result<ObjectRef, MethodCallFailed> {
    let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/ValueLayout", 3)?;
    ctx.set_field(layout, 0, Value::Int(kind));
    ctx.set_field(layout, 1, Value::Int(ffi::layout_byte_size(kind) as i32));
    ctx.set_field(layout, PE_VALUE_LAYOUT_NAME_SLOT, Value::Object(None));
    Ok(layout)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `MemorySegment.spliterator` split arithmetic and characteristics.
#[cfg(test)]
mod segment_splitter_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod w20f_ffm_rooting_tests;

#[cfg(test)]
mod r12w6_ffm3_downcall_acquire_tests;

#[cfg(test)]
mod r12w6_ffm3_heap_bulk_tests;

#[cfg(test)]
mod r12w7_ffm4_reinterpret_tests;

#[cfg(test)]
mod r13w5_ffm3_arena_noinit_tests;

#[cfg(test)]
mod r13w5_ffm3_downcall_group_return_tests;

#[cfg(test)]
mod r13w5_ffm3_prefix_allocator_tests;

#[cfg(test)]
mod r13w13_ffm7_address_read_tests;

/// Round 14 wave 3 (lane ffm, FFM7-1): the `Arena.ofAuto()` block table. The
/// mock context has no weak handles, so these drive the lock-only halves with
/// made-up handles; each test uses its own VM id.
#[cfg(test)]
mod r14w3_ffm_auto_arena_tests;

/// Round 14 wave 4 (lane ffm4, FFM3W-1): rows with pending Java actions. Same
/// made-up handles as `r14w3_ffm_auto_arena_tests`; each test its own VM id.
#[cfg(test)]
mod r14w4_ffm4_auto_arena_action_tests;



/// Round 15 wave 3 (lane ffm2): FFR2-2 (the last action takes its row at once,
/// sweep or no sweep), the closed-shared rows of the deferred shared free, and
/// FFR2-3's cleaner cache. Each test its own VM id.
#[cfg(test)]
mod r15w3_ffm2_tests;

/// JIT round 16 wave 14 (lane ffm3): the lazy in-native downcall's call-site
/// order and the FFS16-1 security-manager test.
#[cfg(test)]
mod r16w14_ffm3_tests;

/// zs s2/nd2: the FFM segment mints that run with nothing raw held -- a
/// downcall's `ADDRESS` return after the C call, an upcall's address argument
/// -- collect and retry on a refusal when `collecting`, then raise the
/// catchable `OutOfMemoryError`; with `collecting == false` they keep the
/// one-attempt door (`zgc-zs1nd-jit-helper-native-doors-20261014.md`).
#[cfg(test)]
mod zs2_nd2_ffm_mint_tests;

/// zt t12/df4: the FFM restricted-method gate names the method HotSpot names
/// (`Linker::upcallStub` for `upcallStub`) and refuses with HotSpot's text
/// under `--illegal-native-access=deny`.
#[cfg(test)]
mod zt12df4_ffm_access_tests;

mod alloc_segment_carrier;
#[allow(unused_imports)]
pub use alloc_segment_carrier::*;
#[allow(unused_imports)]
use alloc_segment_carrier::*;
mod register_pe_memory_segment_on;
#[allow(unused_imports)]
pub use register_pe_memory_segment_on::*;
#[allow(unused_imports)]
use register_pe_memory_segment_on::*;
mod pe_of_array_alias;
#[allow(unused_imports)]
pub use pe_of_array_alias::*;
#[allow(unused_imports)]
use pe_of_array_alias::*;
mod pe_swap_width;
#[allow(unused_imports)]
pub use pe_swap_width::*;
#[allow(unused_imports)]
use pe_swap_width::*;
