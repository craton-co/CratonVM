// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 1 (lane TYPECHK, `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE`; page
//! `docs/known-issues/jit/threadlocal-subclass-overriding-get-or-set-pays-the-thin-helper-decline-20261027.md`):
//! do not bind the `ThreadLocal.get()` / `set(Object)` thin helper at a site
//! whose receiver profile names a dominant class that overrides the method.
//!
//! Every `invokevirtual ThreadLocal.get()` / `set(Object)` under `--jdk-only`
//! binds the thin helper whatever its receiver, because `ThreadLocal` is not
//! final and the helper screens the receiver class itself. For a class that
//! overrides the method (a covariant bridge counts) the helper declines on
//! EVERY call to `jit_invoke_dispatch` with a shared synthetic call-site info:
//! a by-name virtual resolution per call with no inline cache, ~1 us
//! (`R21TlsubRows` `tl-get-override` 15 682 ns per 16 ops against HotSpot's
//! 92). Left an ordinary virtual call, the site keeps its inline cache (or a
//! guarded splice of the override, the profile naming one exact class).
//!
//! The decision reads the same one-class VM query the helper serves a class
//! by (`DirectHelperTable::thread_local_receiver_kind`, the VM's
//! `tl_get_receiver_kind` and `tl_class_declares_set`), so the compiler and
//! the helper cannot disagree about which classes decline. It refuses only on
//! POSITIVE evidence -- a dominant profiled class the VM answers as running
//! its own bytecode -- exactly as `nio_byte_element_bind_refused` does: no
//! profile, no dominant class, or no VM answer binds as before. A wrong
//! refusal costs only speed (the virtual call runs the same JDK code the
//! helper's decline edge would have dispatched to); the answer is unchanged.

use crate::direct_helpers::{
    DirectHelperTable, TL_RECEIVER_KIND_INHERITABLE, TL_RECEIVER_KIND_PLAIN,
};

/// The share (percent of the site's observations) at which the profile's top
/// receiver class decides the bind. Half: an overriding class that takes most
/// of a site's calls makes the bind a loss on most of them.
pub(crate) const TL_THIN_OVERRIDE_DOMINANT_PCT: u32 = 50;

/// `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE` (j27 wave 1, lane TYPECHK; perf,
/// **default ON**, `=0` binds every site as before): see the module doc. Read
/// per site at compile time.
pub fn tl_thin_override_decline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE")
}

/// Does the VM's one-class answer `(map kind, set admitted)` say the thin
/// helper declines class `answer`'s receivers for this method (`is_set`:
/// `set(Object)`, else `get()`)? `get` declines every kind but the two map
/// kinds (the VM's `TL_MAP_BYTECODE`: the class, or a class on its chain below
/// `ThreadLocal`, declares its own `get()`); `set` declines whatever the VM
/// does not admit for `set` (that, or a declared `set(Object)`).
pub(crate) fn thread_local_answer_declines(answer: (u8, bool), is_set: bool) -> bool {
    let (kind, set_ok) = answer;
    if is_set {
        !set_ok
    } else {
        kind != TL_RECEIVER_KIND_PLAIN && kind != TL_RECEIVER_KIND_INHERITABLE
    }
}

/// Should the thin `ThreadLocal.get()` / `set(Object)` bind at `pc` be
/// REFUSED (`is_set` picks the method)? `true` only when the site's profiled
/// receivers name a dominant class ([`TL_THIN_OVERRIDE_DOMINANT_PCT`]) that
/// the VM answers as declined ([`thread_local_answer_declines`]). `door` names
/// the bind site for `CRATONVM_DBG_JITC`.
pub(crate) fn thread_local_thin_bind_refused(
    direct_helpers: &DirectHelperTable,
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
    is_set: bool,
    door: &str,
) -> bool {
    let counts = profile.and_then(|prof| prof.receivers.get(&pc));
    receiver_counts_refuse_thin_bind(direct_helpers, counts, pc, is_set, door)
}

/// [`thread_local_thin_bind_refused`] on one site's receiver counts, wherever
/// they come from (the compiling method's profile at its own pc, or a spliced
/// callee's profile re-keyed at its combined pc).
fn receiver_counts_refuse_thin_bind(
    direct_helpers: &DirectHelperTable,
    counts: Option<&crate::profile::ReceiverCounts>,
    pc: usize,
    is_set: bool,
    door: &str,
) -> bool {
    if !tl_thin_override_decline_enabled() {
        return false;
    }
    let dominant = counts.and_then(|counts| {
        crate::profile::dominant_receiver(counts, TL_THIN_OVERRIDE_DOMINANT_PCT)
    });
    let answer = dominant.and_then(|d| direct_helpers.thread_local_receiver_kind(d));
    let refused = answer.is_some_and(|a| thread_local_answer_declines(a, is_set));
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] tl-thin-bind door={door} pc={pc} set={is_set} \
             dominant={dominant:?} answer={answer:?} {}",
            if refused { "REFUSED (the class overrides)" } else { "bound" }
        );
    }
    refused
}

/// Which `ThreadLocal` method a bound thin `entry` serves (`Some(true)`:
/// `set(Object)`, `Some(false)`: `get()`), or `None` for any other entry.
fn thin_entry_is_set(direct_helpers: &DirectHelperTable, entry: usize) -> Option<bool> {
    if entry == 0 {
        None
    } else if entry == direct_helpers.thread_local_get {
        Some(false)
    } else if entry == direct_helpers.thread_local_set {
        Some(true)
    } else {
        None
    }
}

/// j27 wave 2 (lane TCARRAY): the SPLICED-body half of the same refusal.
/// `append_ir_inline_site` binds a spliced body's surviving
/// `ThreadLocal.get()` / `set(Object)` to the thin helper by name
/// (`spliced_thread_local_thin_cell`), with no profile at hand; the callee's
/// receiver counts for that call ride beside it, re-keyed at its combined pc
/// (`seeds`, `IrInlineTables::spliced_receiver_seeds`). Each bound row in
/// `direct_calls` whose counts [`receiver_counts_refuse_thin_bind`] becomes
/// what it would have been without the thin row: a virtual call with its own
/// inline cache (`virtual_calls`, its argument count from `invoke_info`),
/// seeded from the same counts. Every other row is kept untouched.
pub(crate) fn unbind_refused_spliced_thin_calls(
    direct_helpers: &DirectHelperTable,
    direct_calls: &mut Vec<(usize, (usize, bool))>,
    virtual_calls: &mut Vec<(usize, usize)>,
    seeds: &std::collections::HashMap<usize, crate::profile::ReceiverCounts>,
    invoke_info: &std::collections::HashMap<usize, (usize, usize, u8)>,
) {
    direct_calls.retain(|&(pc, (entry, _))| {
        let Some(is_set) = thin_entry_is_set(direct_helpers, entry) else {
            return true;
        };
        let Some(&(_, num_jit_args, _)) = invoke_info.get(&pc) else {
            return true;
        };
        if !receiver_counts_refuse_thin_bind(direct_helpers, seeds.get(&pc), pc, is_set, "splice") {
            return true;
        }
        virtual_calls.push((pc, num_jit_args));
        false
    });
}

#[cfg(test)]
mod j27_tl_thin_override_tests {
    use super::*;

    /// `get` declines the bytecode kind only; `set` declines what the VM does
    /// not admit for `set`.
    #[test]
    fn j27_the_vm_answer_decides_per_method() {
        // The VM's `TL_MAP_BYTECODE`.
        const BYTECODE: u8 = 2;
        assert!(!thread_local_answer_declines((TL_RECEIVER_KIND_PLAIN, true), false));
        assert!(!thread_local_answer_declines((TL_RECEIVER_KIND_INHERITABLE, true), false));
        assert!(thread_local_answer_declines((BYTECODE, false), false));
        assert!(!thread_local_answer_declines((TL_RECEIVER_KIND_PLAIN, true), true));
        // A `get`-only override still serves `set` when the VM admits it.
        assert!(!thread_local_answer_declines((BYTECODE, true), true));
        assert!(thread_local_answer_declines((TL_RECEIVER_KIND_PLAIN, false), true));
    }

    /// No profile binds (positive evidence only), and the switch is default
    /// ON with `=0` as its kill word.
    #[test]
    fn j27_no_profile_binds_and_the_switch_defaults_on() {
        let key = "CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE";
        let table = DirectHelperTable::default();
        cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            assert!(tl_thin_override_decline_enabled());
            assert!(!thread_local_thin_bind_refused(&table, None, 7, false, "test"));
            assert!(!thread_local_thin_bind_refused(&table, None, 7, true, "test"));
        });
        cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
            assert!(!tl_thin_override_decline_enabled());
        });
    }

    /// The class [`fake_receiver_query`] answers as overriding `get` and `set`.
    const OVERRIDING: u32 = 0x77;
    const PLAIN_CLASS: u32 = 0x78;
    const GET: usize = 0x1000;
    const SET: usize = 0x2000;
    const OTHER: usize = 0x3000;

    /// The VM's one-class query, faked: [`OVERRIDING`] is the bytecode kind
    /// and not set-admitted, every other class the plain kind.
    unsafe extern "C" fn fake_receiver_query(_ctx: i64, io: i64, words: i64) -> i64 {
        // SAFETY: `thread_local_receiver_kind` hands a `words`-word buffer.
        let io = unsafe { std::slice::from_raw_parts_mut(io as *mut u64, words as usize) };
        let overrides = io[0] == u64::from(OVERRIDING);
        io[0] = if overrides { 2 } else { u64::from(TL_RECEIVER_KIND_PLAIN) };
        io[1] = u64::from(!overrides);
        words
    }

    fn fake_table() -> DirectHelperTable {
        DirectHelperTable {
            thread_local_get: GET,
            thread_local_set: SET,
            thread_local_ir_layout: fake_receiver_query as *const () as usize,
            static_base_resolver_ctx: 1,
            ..DirectHelperTable::default()
        }
    }

    /// Spliced rows `(combined pc, entry)` after the pass, and the virtual
    /// calls it made, under the switch value `switch`.
    fn unbind(switch: Option<&str>) -> (Vec<(usize, (usize, bool))>, Vec<(usize, usize)>) {
        let table = fake_table();
        let hot = |class: u32| -> crate::profile::ReceiverCounts { [(class, 100)].into_iter().collect() };
        let seeds: std::collections::HashMap<usize, crate::profile::ReceiverCounts> =
            [(10, hot(OVERRIDING)), (20, hot(PLAIN_CLASS)), (30, hot(OVERRIDING)), (40, hot(OVERRIDING))]
                .into_iter()
                .collect();
        let info: std::collections::HashMap<usize, (usize, usize, u8)> =
            [10, 20, 30, 40, 50].into_iter().map(|pc| (pc, (0x9, 1, 0))).collect();
        // pc 50: an overriding-get row whose counts were never seeded.
        let mut direct = vec![(10, (GET, true)), (20, (GET, true)), (30, (SET, true)), (40, (OTHER, true)), (50, (GET, true))];
        let mut vcalls: Vec<(usize, usize)> = Vec::new();
        let key = "CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE";
        cratonvm_types::flags::with_thread_overrides(&[(key, switch)], || {
            unbind_refused_spliced_thin_calls(&table, &mut direct, &mut vcalls, &seeds, &info);
        });
        (direct, vcalls)
    }

    /// A spliced thin `get` / `set` whose callee profile names an overriding
    /// class becomes a virtual call; the plain class, a non-`ThreadLocal`
    /// entry and an unprofiled row stay bound; `=0` keeps every bind.
    #[test]
    fn j27_a_spliced_thin_call_on_an_overriding_class_becomes_a_virtual_call() {
        let (direct, vcalls) = unbind(None);
        assert_eq!(direct, vec![(20, (GET, true)), (40, (OTHER, true)), (50, (GET, true))]);
        assert_eq!(vcalls, vec![(10, 1), (30, 1)]);
        let (direct, vcalls) = unbind(Some("0"));
        assert_eq!(direct.len(), 5);
        assert!(vcalls.is_empty());
    }
}
