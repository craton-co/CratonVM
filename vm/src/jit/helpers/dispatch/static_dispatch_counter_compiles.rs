#[allow(unused_imports)]
use super::*;


/// `CRATONVM_JIT_DISPATCH_COUNTER_VIA_MANAGER` (j27 wave 2, lane NATMISC;
/// **default ON**, `=0` is the kill switch; page
/// `j27-static-dispatch-counter-compiles-on-its-own-clock-FIXED-20261029.md`).
///
/// The static dispatch tail's `DISPATCH_COUNTER` counts per thread and per
/// CALL SITE, and at its threshold asked the callee door for a C2 body: by
/// default that door defers a C2 task to the worker
/// (`CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER`), otherwise it compiles on
/// the mutator; either way at this per-site clock, outside the tiered
/// manager's policy and the profile store. HotSpot keeps
/// one invocation counter per method and asks its compilation policy. Under
/// `CRATONVM_BG_COMPILE` (default on) the tail's own doors already count the
/// method and offer it to the manager (the template door through
/// `bytecode_callee_compiled_or_nominate`, the by-name tail through
/// `execute`'s background count), and the `jit_cache` probe above binds
/// `DISPATCH_CACHE` once the manager publishes a body. So the inline compile
/// is kept only for `CRATONVM_BG_COMPILE=0` (every nomination site's inline
/// mode) or with this switch killed. Read once per site per thread, at the
/// counter's crossing.
pub fn static_dispatch_counter_compiles_inline() -> bool {
    dispatch_counter_compile_is_inline(
        crate::runtime::env_cache::bg_compile(),
        true,
    )
}

/// The decision of [`static_dispatch_counter_compiles_inline`]: inline only
/// when compiles are not backgrounded, or when routing through the manager
/// is switched off.
pub fn dispatch_counter_compile_is_inline(bg_compile: bool, via_manager: bool) -> bool {
    !(bg_compile && via_manager)
}

/// `CRATONVM_JIT_VIRTUAL_DISPATCH_COUNTER_PROBE_ONLY` (j27 wave 3, lane
/// DISPATCH2; **default ON**, `=0` is the kill switch; page
/// `j27-static-dispatch-counter-compiles-on-its-own-clock-FIXED-20261029.md`): the
/// virtual twin of [`static_dispatch_counter_compiles_inline`].
///
/// The blind virtual arm's `VIRTUAL_DISPATCH_COUNTER` crossing (its threshold,
/// the doubling re-probes, the megamorphic table hint, a stale body dropped)
/// asks the receiver-keyed callee door. That ask cannot simply be skipped, as
/// the static one is: its probe is the only place the arm binds a published
/// body into `VIRTUAL_DISPATCH_CACHE`. So under the background compiler the
/// door is asked in probe-only mode ([`CalleeDoorMiss::ProbeOnly`]: every
/// filter on a published body, `None` where it would defer a C2 task at this
/// per-site, per-thread clock or compile), and the method's compile is left
/// to the tiered manager, which the call's tail counts and nominates. With
/// `CRATONVM_BG_COMPILE=0` or `=0` here the door defers or compiles as before.
/// Read at the crossing only.
pub(crate) fn virtual_dispatch_counter_door_miss(
) -> crate::runtime::interpreter::jit_bridge::CalleeDoorMiss {
    virtual_dispatch_counter_door_miss_for(crate::runtime::env_cache::bg_compile(), true)
}

/// The decision of [`virtual_dispatch_counter_door_miss`], on the static
/// twin's rule ([`dispatch_counter_compile_is_inline`]).
pub(crate) fn virtual_dispatch_counter_door_miss_for(
    bg_compile: bool,
    probe_only: bool,
) -> crate::runtime::interpreter::jit_bridge::CalleeDoorMiss {
    use crate::runtime::interpreter::jit_bridge::CalleeDoorMiss;
    if dispatch_counter_compile_is_inline(bg_compile, probe_only) {
        CalleeDoorMiss::MayDefer
    } else {
        CalleeDoorMiss::ProbeOnly
    }
}

