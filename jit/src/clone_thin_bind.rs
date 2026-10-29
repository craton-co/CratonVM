// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 3 (lane CLONE, `CRATONVM_JIT_CLONE_VIRTUAL_THIN`; page
//! `docs/known-issues/jit/javac-zipfs-typecheck-sites-and-instance-clone-leave-compiled-code-20261027.md`
//! item 3): bind a compiled VIRTUAL `clone()` to the thin helper
//! `DirectHelperTable::object_clone_virtual`.
//!
//! A virtual `clone()` whose target is `Object.clone` -- every array's
//! `clone()`, and an instance whose class inherits it -- took
//! `jit_invoke_virtual_mic`'s slow path into the registered native on every
//! call: an array receiver never sits in a monomorphic inline cache, and a
//! native target has no compiled body to cache. HotSpot's compilers expand
//! both inline (C2 `inline_native_clone`: an allocation and a copy). The
//! helper copies them itself (a TLAB bump and the copy, the collector's
//! barriers on a reference payload) and makes the site's own virtual call
//! through the generic dispatcher for every other receiver (an override, a
//! class that is not `Cloneable`, a finalizable one, a copy the TLAB cannot
//! take), so `CloneNotSupportedException` and every non-plain case keep the
//! native's answer.
//!
//! Which sites bind:
//!
//! * an array class (`[I`, `[Ljava/lang/String;`, `[[J`, ...): always -- the
//!   target is `Object.clone` whatever the receiver. `[B` keeps its own
//!   helper and switch (`byte_array_clone_helper`), asked first at every door.
//! * any other class: only on POSITIVE evidence -- the site's receiver profile
//!   names a dominant class ([`CLONE_VIRTUAL_DOMINANT_PCT`]) that the VM
//!   answers as served (`DirectHelperTable::object_clone_class_is_served`, the
//!   one-class question the helper itself asks per call). No profile, no
//!   dominant class or an overriding one keeps the inline cache (or the
//!   guarded splice of the override), as before: a wrong bind would cost one
//!   generic dispatch per call, never a wrong answer.
//!
//! A spliced body's surviving call is asked twice. The splice's thin closure
//! (`spliced_thin_helper`) has no receiver counts, so it binds an array class
//! only. j27 wave 4 (lane CLONE2, `CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND`):
//! after the splice, [`bind_served_spliced_clone_calls`] asks the same
//! question of each surviving VIRTUAL `clone()` with the CALLEE's receiver
//! counts for that call (`IrInlineTables::spliced_receiver_seeds`, re-keyed at
//! its combined pc), so an instance `x.clone()` inside a spliced callee binds
//! on the same positive evidence as one in the compiling method.

use std::collections::HashMap;

use crate::direct_helpers::DirectHelperTable;

/// The share (percent of the site's observations) the profile's top receiver
/// class must hold for an instance site to bind. High, as for the `ByteBuffer`
/// element bind: every other receiver pays the helper's decline into the
/// generic dispatcher, which is slower than the inline cache it replaces.
pub(crate) const CLONE_VIRTUAL_DOMINANT_PCT: u32 = 80;

const CLONE_DESCRIPTOR: &str = "()Ljava/lang/Object;";

/// `CRATONVM_JIT_CLONE_VIRTUAL_THIN` (j27 wave 3, lane CLONE; perf,
/// **default ON**, `=0` restores the inline-cache route at every door). Read
/// per candidate site at compile time.
pub fn clone_virtual_thin_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CLONE_VIRTUAL_THIN")
}

/// The thin helper for a virtual `class.method descriptor` site, or `0`.
/// `counts` are the site's receiver counts (`None`: unprofiled, or a spliced
/// body's call, where only an array class can bind). `door` names the bind
/// site for `CRATONVM_DBG_JITC`. The strict-mode admission is the registry's
/// answer about `Object.clone`, the native the helper stands in front of.
pub(crate) fn virtual_clone_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    counts: Option<&crate::profile::ReceiverCounts>,
    (class, method, descriptor): (&str, &str, &str),
    door: &str,
) -> usize {
    if method != "clone"
        || descriptor != CLONE_DESCRIPTOR
        || class == "[B"
        || !clone_virtual_thin_enabled()
    {
        return 0;
    }
    let (evidence, dominant) = site_evidence(table, class, counts);
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] clone-virtual-bind door={door} class={class} \
             dominant={dominant:?} {evidence:?}"
        );
    }
    if !evidence.binds() {
        return 0;
    }
    crate::direct_native_helper(
        table.object_clone_virtual,
        jdk_only,
        direct_bind_admission,
        "java/lang/Object",
        "clone",
        CLONE_DESCRIPTOR,
    )
}

/// [`virtual_clone_helper`] at a site of the compiling method itself, whose
/// receiver counts are `profile`'s at `pc`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn virtual_clone_helper_at(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
    site: (&str, &str, &str),
    door: &str,
) -> usize {
    let counts = profile.and_then(|prof| prof.receivers.get(&pc));
    virtual_clone_helper(table, jdk_only, direct_bind_admission, counts, site, door)
}

/// `CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND` (j27 wave 4, lane CLONE2; perf,
/// **default ON**, `=0` leaves every spliced instance `clone()` on its inline
/// cache, as wave 3 did). Read once per spliced site at compile time. Under
/// `CRATONVM_JIT_CLONE_VIRTUAL_THIN=0` nothing binds either way.
pub fn clone_spliced_instance_bind_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND")
}

/// j27 wave 4 (lane CLONE2): bind the spliced bodies' surviving VIRTUAL
/// `clone()` calls from the callee's receiver counts. Each row of
/// `virtual_calls` (`(combined pc, argument words)`) whose interned call
/// (`invoke_info`) is `invokevirtual ...clone()Ljava/lang/Object;` and whose
/// counts (`seeds`) [`virtual_clone_helper`] binds becomes a thin row of
/// `direct_calls` (`(pc, (entry, needs context))`), and takes no inline cache.
/// Every other row is kept untouched.
pub(crate) fn bind_served_spliced_clone_calls(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    direct_calls: &mut Vec<(usize, (usize, bool))>,
    virtual_calls: &mut Vec<(usize, usize)>,
    seeds: &HashMap<usize, crate::profile::ReceiverCounts>,
    invoke_info: &HashMap<usize, (usize, usize, u8)>,
) {
    if !clone_spliced_instance_bind_enabled() {
        return;
    }
    virtual_calls.retain(|&(pc, _)| {
        let Some(site) = invoke_info.get(&pc).and_then(|row| interned_virtual_clone_site(row.0))
        else {
            return true;
        };
        let counts = seeds.get(&pc);
        let entry =
            virtual_clone_helper(table, jdk_only, direct_bind_admission, counts, site, "splice-seed");
        if entry == 0 {
            return true;
        }
        direct_calls.push((pc, (entry, true)));
        false
    });
}

/// The `(class, method, descriptor)` of the interned call at `info_ptr` when
/// it is a VIRTUAL `clone()` (`invoke_kind` 0: never an `invokespecial`,
/// whose `super.clone()` has its own helper), else `None`.
fn interned_virtual_clone_site(
    info_ptr: usize,
) -> Option<(&'static str, &'static str, &'static str)> {
    if info_ptr == 0 {
        return None;
    }
    // SAFETY: a non-zero address in `IrInlineTables::invoke_info` is a
    // `JitInvokeInfo` this compile interned (`owned_invoke_infos`), alive for
    // the compile and baked into the artifact (`plan_spliced_ffm_element_sites`
    // reads the same rows).
    let info = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
    (info.invoke_kind == 0 && info.method_name == "clone" && info.descriptor == CLONE_DESCRIPTOR)
        .then_some((info.class_name, info.method_name, info.descriptor))
}

/// What a virtual `clone()` site's receivers say about the bind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloneSiteEvidence {
    /// An array class: the target is `Object.clone`.
    Array,
    /// No dominant receiver class (no profile, too few samples, a mix).
    NoDominantClass,
    /// The dominant class, served by the helper.
    Served,
    /// The dominant class, not served (an override, not plain).
    NotServed,
}

impl CloneSiteEvidence {
    fn binds(self) -> bool {
        matches!(self, Self::Array | Self::Served)
    }
}

/// The site's evidence, and the dominant receiver class it was read from.
fn site_evidence(
    table: &DirectHelperTable,
    class: &str,
    counts: Option<&crate::profile::ReceiverCounts>,
) -> (CloneSiteEvidence, Option<u32>) {
    if class.starts_with('[') {
        return (CloneSiteEvidence::Array, None);
    }
    let dominant =
        counts.and_then(|c| crate::profile::dominant_receiver(c, CLONE_VIRTUAL_DOMINANT_PCT));
    let evidence = match dominant {
        None => CloneSiteEvidence::NoDominantClass,
        Some(d) if table.object_clone_class_is_served(d) => CloneSiteEvidence::Served,
        Some(_) => CloneSiteEvidence::NotServed,
    };
    (evidence, dominant)
}

#[cfg(test)]
mod j27_clone_virtual_thin_tests {
    use super::*;

    const ENTRY: usize = 0x0c1e;
    const SERVED_CLASS: u32 = 0x51;
    const OVERRIDING_CLASS: u32 = 0x52;
    const KEY: &str = "CRATONVM_JIT_CLONE_VIRTUAL_THIN";

    /// The VM's one-class question, faked: [`SERVED_CLASS`] is served.
    unsafe extern "C" fn fake_served(ctx: i64, class_id: i64) -> i64 {
        i64::from(ctx == 1 && class_id == i64::from(SERVED_CLASS))
    }

    fn table() -> DirectHelperTable {
        DirectHelperTable {
            object_clone_virtual: ENTRY,
            object_clone_served_class: fake_served as *const () as usize,
            static_base_resolver_ctx: 1,
            ..DirectHelperTable::EMPTY
        }
    }

    fn hot(class: u32) -> crate::profile::ReceiverCounts {
        [(class, 100)].into_iter().collect()
    }

    fn bind(class: &str, counts: Option<&crate::profile::ReceiverCounts>, admits: bool) -> usize {
        let admission = move |_: &str, _: &str, _: &str| admits;
        let site = (class, "clone", CLONE_DESCRIPTOR);
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            virtual_clone_helper(&table(), true, Some(&admission), counts, site, "test")
        })
    }

    /// Every array class binds with no profile at all; `[B` is left to its
    /// own helper.
    #[test]
    fn j27_an_array_clone_binds_without_evidence() {
        for class in ["[I", "[J", "[Z", "[D", "[Ljava/lang/String;", "[[I"] {
            assert_eq!(bind(class, None, true), ENTRY, "{class}");
        }
        assert_eq!(bind("[B", None, true), 0);
    }

    /// An instance site binds only when its dominant class is served.
    #[test]
    fn j27_an_instance_clone_binds_only_on_a_served_dominant_class() {
        let class = "java/util/TimeZone";
        assert_eq!(bind(class, None, true), 0, "unprofiled");
        assert_eq!(bind(class, Some(&hot(SERVED_CLASS)), true), ENTRY);
        assert_eq!(bind(class, Some(&hot(OVERRIDING_CLASS)), true), 0);
        let mixed: crate::profile::ReceiverCounts =
            [(SERVED_CLASS, 50), (OVERRIDING_CLASS, 50)].into_iter().collect();
        assert_eq!(bind(class, Some(&mixed), true), 0, "no dominant class");
    }

    /// The registry's refusal and another method are honoured; `=0` unbinds
    /// every site.
    #[test]
    fn j27_the_registry_the_method_and_the_kill_switch_decide_too() {
        assert_eq!(bind("[I", None, false), 0);
        let admission = |_: &str, _: &str, _: &str| true;
        let hash = virtual_clone_helper(
            &table(),
            true,
            Some(&admission),
            None,
            ("[I", "hashCode", "()I"),
            "test",
        );
        assert_eq!(hash, 0);
        let off = cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            virtual_clone_helper(
                &table(),
                true,
                Some(&admission),
                None,
                ("[I", "clone", CLONE_DESCRIPTOR),
                "test",
            )
        });
        assert_eq!(off, 0);
    }

    const fn call(
        class_name: &'static str,
        method_name: &'static str,
        kind: u8,
    ) -> crate::JitInvokeInfo {
        crate::JitInvokeInfo {
            class_name,
            method_name,
            descriptor: CLONE_DESCRIPTOR,
            num_jit_args: 1,
            return_type: b'L',
            invoke_kind: kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    static VIRTUAL_CLONE: crate::JitInvokeInfo = call("java/util/TimeZone", "clone", 0);
    static SUPER_CLONE: crate::JitInvokeInfo = call("java/util/TimeZone", "clone", 1);
    static OTHER_METHOD: crate::JitInvokeInfo = call("java/util/TimeZone", "toClone", 0);

    /// The spliced rows after [`bind_served_spliced_clone_calls`] under the
    /// splice switch value `switch`: `(direct rows, virtual rows left)`.
    fn splice(switch: Option<&str>) -> (Vec<(usize, (usize, bool))>, Vec<(usize, usize)>) {
        let addr =
            |info: &'static crate::JitInvokeInfo| info as *const crate::JitInvokeInfo as usize;
        let seeds: HashMap<usize, crate::profile::ReceiverCounts> = [
            (10, hot(SERVED_CLASS)),
            (20, hot(OVERRIDING_CLASS)),
            (40, hot(SERVED_CLASS)),
            (50, hot(SERVED_CLASS)),
            (60, hot(SERVED_CLASS)),
        ]
        .into_iter()
        .collect();
        // pc 30: a served-looking call whose callee profile never saw it.
        // pc 60: no interned row at all.
        let info: HashMap<usize, (usize, usize, u8)> = [
            (10, addr(&VIRTUAL_CLONE)),
            (20, addr(&VIRTUAL_CLONE)),
            (30, addr(&VIRTUAL_CLONE)),
            (40, addr(&SUPER_CLONE)),
            (50, addr(&OTHER_METHOD)),
        ]
        .into_iter()
        .map(|(pc, a)| (pc, (a, 1, b'L')))
        .collect();
        let mut direct = vec![(70, (0x7070, true))];
        let mut vcalls: Vec<(usize, usize)> =
            [10, 20, 30, 40, 50, 60].into_iter().map(|pc| (pc, 1)).collect();
        let admission = |_: &str, _: &str, _: &str| true;
        let keys = [(KEY, None), ("CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND", switch)];
        cratonvm_types::flags::with_thread_overrides(&keys, || {
            bind_served_spliced_clone_calls(
                &table(),
                true,
                Some(&admission),
                &mut direct,
                &mut vcalls,
                &seeds,
                &info,
            );
        });
        (direct, vcalls)
    }

    /// j27 wave 4 (lane CLONE2): a spliced instance `clone()` whose callee
    /// profile names a served class becomes the thin row and leaves the
    /// inline-cache list; an overriding or unprofiled class, an
    /// `invokespecial`, another method and an uninterned row keep their call;
    /// the rows already there are kept; `=0` binds nothing.
    #[test]
    fn j27_a_spliced_instance_clone_binds_from_the_callee_profile() {
        let (direct, vcalls) = splice(None);
        assert_eq!(direct, vec![(70, (0x7070, true)), (10, (ENTRY, true))]);
        assert_eq!(vcalls, vec![(20, 1), (30, 1), (40, 1), (50, 1), (60, 1)]);
        let (direct, vcalls) = splice(Some("0"));
        assert_eq!(direct, vec![(70, (0x7070, true))]);
        assert_eq!(vcalls.len(), 6);
    }

    /// Not wired (no VM context, or no question): no instance class is served.
    #[test]
    fn j27_an_unwired_question_serves_nothing() {
        let unwired = DirectHelperTable {
            static_base_resolver_ctx: 0,
            ..table()
        };
        assert!(!unwired.object_clone_class_is_served(SERVED_CLASS));
        assert!(table().object_clone_class_is_served(SERVED_CLASS));
        assert!(!table().object_clone_class_is_served(0));
    }
}
