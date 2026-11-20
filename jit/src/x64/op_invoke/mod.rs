// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Invocations: `invokevirtual`, `invokespecial`, `invokestatic`, `invokeinterface` and `invokedynamic` in the single-pass backend's bytecode walk.
//!
//! Split out of `Compiler::compile_bytecode` (`bytecode_walk.rs`), which keeps
//! the walk loop and one dispatch `match` that routes each opcode to its
//! family (`jit-god-functions-and-request-side-channels-FIXED-20260912.md`).
//! The arms are the walk's own, moved unchanged except for how they leave the
//! walk: `continue` became `return WalkStep::Next(pc)` and `return x` became
//! `return WalkStep::Return(x)`.

use super::bytecode_walk::*;
use super::*;
/// nio21 wave 11 L5: the single-pass handle proof, executed against a fake
/// statics block.
#[cfg(all(test, target_arch = "x86_64"))]
mod nio21w11_vh_const_handle_proof_tests;
/// gcd d10/f (2026-09-28): the replay rule every other frameless exit of a
/// single-pass body is held to before install (`x64/driver.rs`,
/// `deopt::single_pass_first_trap_needing_unsound_replay`), asked of the call
/// at `pc`: may a re-run of this method from bci 0 replace an attempt that
/// stopped at this call? `true` when nothing in `code[..pc]`, closed over
/// every loop containing `pc`, commits a side effect (no array or field
/// store, no invoke, no monitor operation:
/// `deopt::replay_from_entry_commits_nothing_with_handlers` with the
/// single-pass tier's "spliced bodies are pure", since a splice's `invoke` is
/// in this prefix, and `handlers`, the method's exception table in bci space:
/// a backward handler edge widens the extent exactly as it does for the
/// driver's frameless traps).
///
/// `code`/`code_len` are the bytecode the walk emits. It answers `false`
/// (keep the copy) for a compile of REWRITTEN bytecode, whose pcs are not the
/// interpreter's bcis the rule is stated in, and for a self-locking body,
/// whose frameless exit would release and re-take the method monitor across
/// the re-run.
///
/// The residue, which this does not change: the callee's own abandoned
/// attempt is still replayed by the re-run, exactly as the copying site's
/// `rerun_declined_callee_from_entry` / frameless re-run replays it (counted
/// in the door census, `callee-declined`).
///
/// j27 wave 9: production asks [`caller_replay_to_call_is_exact_with_statics`]
/// (this rule with no static vouched for); this form stays for the gcd d9/d tests.
#[cfg(test)]
fn caller_replay_to_call_is_exact(
    code: &[u8],
    code_len: usize,
    pc: usize,
    handlers: &[(usize, usize, usize)],
) -> bool {
    // ARGWIN21-4: plus a straight-line prefix whose only stores fill arrays
    // it allocated itself (`deopt::owned_args_caller_replay_is_exact`).
    u32::try_from(pc).is_ok_and(|bci| {
        crate::deopt::owned_args_caller_replay_is_exact(code, code_len, bci, handlers)
    })
}

#[cfg(test)]
impl SbToStringPlan {
    /// A hand-built recipe for executed-code tests of the allocation tails
    /// (JIT round 19 proposal wave 5, lane concat5): `total`-byte String with
    /// `value` at `value_off`, class `cid`, the replace guard at `guard`, and
    /// the real header offsets and `byte[]` mark word.
    pub(super) fn for_test(cid: u32, total: i32, value_off: i32, guard: (usize, u32)) -> Self {
        use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};
        let mark = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Array,
            ArrayElementType::Byte,
            0,
            0,
        )
        .mark_word
        .load(std::sync::atomic::Ordering::Relaxed);
        let d8 = |v: usize| u8::try_from(v).expect("a disp8 header constant");
        Self {
            string_class_id: cid,
            string_total: total,
            value_off,
            replace_guard: guard,
            max_count: 4096,
            len_d8: d8(cratonvm_types::ARRAY_LENGTH_OFFSET),
            data_d8: d8(cratonvm_types::ARRAY_DATA_OFFSET),
            kind_d8: d8(cratonvm_types::KIND_TAGS_BYTE_OFFSET),
            byte_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[B").unwrap_or(0),
            // Cast: the mark word's bits as an imm32.
            byte_array_mark: mark as i32,
            utf16_coder: None,
        }
    }
}



#[cfg(test)]
mod r11w12_hashmap_ic_stack_args;

#[cfg(test)]
mod r13w11_mega9_kind_screen_tests;



#[cfg(test)]
mod gcd_d9d_owned_args_site_tests;

#[cfg(test)]
mod r16w12_throw3_string_null_receiver;

/// JIT round 17 wave 1, lane strings (STR16-P1): the compile-time plan of the
/// inline `StringBuilder.toString()` refuses a String layout with no class id
/// whatever the environment (the switch off is the first refusal, the missing
/// id the next), so such a site can only ever emit its decline.
#[cfg(test)]
mod r17_strings_sb_to_string_plan_tests;

/// nio21 wave 10 L1 (`CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`): the inline
/// `StringBuilder.toString()` under ZGC sets both start bits BEFORE its
/// cursor commit (`=0`: after it), read from the emitted bytes; and the
/// single-pass box sites hand the announce to the decline form.
#[cfg(test)]
mod nio21w10_sb_to_string_start_bits_tests;





#[cfg(test)]
mod i59_l2_splice_post_call_exit_tests;





#[cfg(test)]
mod nio21_w3_sp_scoped_access_prefix_tests;

#[cfg(test)]
mod nio21_w1_sp_identity_hash_prefix_tests;



#[cfg(test)]
mod nio20_w4_sp_current_thread_prefix_tests;

#[cfg(test)]
mod nio20_w3_sp_thin_prefix_table_tests;

/// JIT round 19 proposal wave 8, lane tail8: TAIL7-19-1 (the lazy return-tail
/// republish over a VM-free window) and TAIL7-19-3 (the instance-arm leaf
/// twins and the wide decline compare).
#[cfg(test)]
mod tail8_tests;

#[cfg(test)]
mod gci_i1sp_zero_dead_arg_sources_tests;













mod sp_tailcall_keeps_vm_helper_frame;
#[allow(unused_imports)]
pub use sp_tailcall_keeps_vm_helper_frame::*;
#[allow(unused_imports)]
use sp_tailcall_keeps_vm_helper_frame::*;
mod sp_byte_array_view_body;
#[allow(unused_imports)]
pub use sp_byte_array_view_body::*;
#[allow(unused_imports)]
use sp_byte_array_view_body::*;
mod walk_invoke_instance;
#[allow(unused_imports)]
pub use walk_invoke_instance::*;
#[allow(unused_imports)]
use walk_invoke_instance::*;
mod arraylist_access_prefix;
#[allow(unused_imports)]
pub use arraylist_access_prefix::*;
#[allow(unused_imports)]
use arraylist_access_prefix::*;
mod indy_site_is_bridged;
#[allow(unused_imports)]
pub use indy_site_is_bridged::*;
#[allow(unused_imports)]
use indy_site_is_bridged::*;
mod sb_to_string_latin1_body;
#[allow(unused_imports)]
pub use sb_to_string_latin1_body::*;
#[allow(unused_imports)]
use sb_to_string_latin1_body::*;
