// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
#[cfg(test)]
mod j27_mh_real_bound_member_kind_tests;
#[cfg(test)]
mod jr20_mh_bind_return_class_tests;
#[cfg(test)]
mod i91_l4_field_link_site_local_tests;
#[cfg(test)]
thread_local! {
    /// Unit tests only: the VM identity the memo uses with the switch taken as
    /// on, so the mock context (identity `0`) can exercise it.
    pub static FIELD_LINK_MEMO_TEST_VM: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Wave 70 (`CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO`): write the remembered
/// link on `this` (which passed [`field_link_candidate`] as `candidate`) when
/// its key's line holds one and [`field_link_memo_write`] admits this call.
/// `judge_site`: the call site must be the interned declared type (`invoke`'s
/// link always, `invokeExact`'s under `CRATONVM_MH_STRICT_INVOKEEXACT`, as
/// the derivation judges it). Performs no allocation and no Java call.
/// The production callers take [`field_link_memo_replay_noting`] (wave 71).
#[cfg(test)]
pub fn field_link_memo_replay(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    candidate: FieldLinkCandidate,
    extra: &[Value],
    call_site: Option<&str>,
    judge_site: bool,
) -> FieldLinkMemoReplay {
    field_link_memo_replay_noting(
        ctx, this, candidate, extra, call_site, judge_site, &mut None,
    )
}

/// Does `bindTo`'s ordinary mint of `this` take [`alloc_bound_method_handle`]
/// (MH2-3)? Only for the kinds whose bound `type` [`bindto_bound_type_descriptor`]
/// knows, and only for a handle of this model's layout (a readable `MH_KIND`
/// and a string object in each of `MH_CLASS` / `MH_NAME` / `MH_DESC`, which
/// the copy shares; anything else keeps [`alloc_method_handle`], which writes
/// `""` for a missing one). Reads only. (`bindTo` itself asks
/// [`bindto_shared_mint_applies_read`] with the strings it already decoded.)
#[cfg(test)]
pub fn bindto_shared_mint_applies(ctx: &dyn NativeContext, this: ObjectRef, kind: i32) -> bool {
    let strings_read =
        [MH_CLASS, MH_NAME, MH_DESC]
            .into_iter()
            .all(|slot| match ctx.get_field(this, slot) {
                Value::Object(Some(s)) => ctx.read_string(s).is_some(),
                _ => false,
            });
    bindto_shared_mint_applies_read(ctx, this, kind, strings_read)
}

/// The `type` descriptor `bindTo`'s ordinary mint gives the bound handle: for a
/// virtual / special handle its stamped type minus the receiver when it had one
/// (`bound_virtual_type`), else `MH_DESC` (receiver-less); for the other kinds
/// the handle's `type` minus its leading parameter, or `MH_DESC` when `type`
/// does not read. The same answers the two post-mint rebuilds in `bindTo` give
/// (which the shared mint skips). `this` must be current; reads only.
#[cfg(test)]
pub fn bindto_bound_type_descriptor(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    kind: i32,
    desc: &str,
    bound_virtual_type: Option<&str>,
) -> String {
    bindto_bound_type_descriptor_with(ctx, this, kind, desc, bound_virtual_type, None)
}

#[cfg(test)]
thread_local! {
    /// Unit tests only: the VM identity the fast mint uses with the switch
    /// taken as on, so the mock context (identity `0`) can exercise the memo.
    pub static BINDTO_MINT_TEST_VM: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Unit tests only: fast-mint hits on this thread.
    pub static BINDTO_MINT_TEST_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The first line of `key`'s set in a fresh memo.
#[cfg(test)]
pub fn mt_intern_slot(key: &MtInternKey) -> usize {
    2 * (mt_intern_hash(key) & (MT_INTERN_MEMO_SLOTS / 2 - 1))
}

/// The filed answer for `key` in VM `vm` and collection epoch `epoch`.
#[cfg(test)]
pub fn mt_intern_memo_get(vm: usize, epoch: u64, key: &MtInternKey) -> Option<ObjectRef> {
    MT_INTERN_MEMO.with(|m| m.borrow_mut().get(vm, epoch, key))
}


/// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): a filter species' form
/// key decodes to the filtered position ([`mh_filter_transform_position`]).
#[cfg(test)]
mod jr21_mh_filter_transform_key_tests;

/// j27 wave 2 (lane INVOKE; `CRATONVM_MH_BINDTO_REAL_MINT_BATCH`): the batched
/// memo-hit mint stores what the per-field mint stores -- the prototype's `type`
/// and leading bound arguments (a primitive one included), the line's `form`,
/// and the new value last -- and leaves the prototype alone.
#[cfg(test)]
mod j27_invoke_bind_mint_batch_tests;

#[cfg(test)]
mod j27_mheditor17_real_jdk_handle_tests;

/// JIT tier2 wave 5, lane nat21 (NATIVE21-4): the pure halves of
/// `mh_real_iwa_varargs_direct`'s primitive-component lane.
#[cfg(test)]
mod nat21_iwa_primitive_varargs_tests;

mod mh_direct_lane_enabled;
#[allow(unused_imports)]
pub use mh_direct_lane_enabled::*;
#[allow(unused_imports)]
use mh_direct_lane_enabled::*;
mod mh_bindto_prelink_bound_field;
#[allow(unused_imports)]
pub use mh_bindto_prelink_bound_field::*;
#[allow(unused_imports)]
use mh_bindto_prelink_bound_field::*;
mod method_finder_mint_file;
#[allow(unused_imports)]
pub use method_finder_mint_file::*;
#[allow(unused_imports)]
use method_finder_mint_file::*;
mod mh_invoke_with_arguments_list_door;
#[allow(unused_imports)]
pub use mh_invoke_with_arguments_list_door::*;
#[allow(unused_imports)]
use mh_invoke_with_arguments_list_door::*;
