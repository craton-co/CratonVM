// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 6, lane T1LAM2 (item 1 of
//! `docs/feature-designs/native-dispatch-architecture.md`;
//! `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC` in the VM, default ON
//! since 2026-10-28 (j27 WARNING flip; =0 restores)): how
//! `IrBuilder::mh_constant_bound_opcode` builds a bound constant handle's site.
//!
//! A `static final` REAL `bindTo` result has two shapes:
//!
//! * a VIRTUAL member bound on its receiver (`invoke_kind 4` in the plan,
//!   MHBIND21-1): the member's `invokevirtual` on the handle's `argL1`, behind
//!   a null guard that deopts at the `invokeExact`;
//! * a STATIC member bound on its leading reference parameter (`invoke_kind 6`
//!   in the plan, this lane): the member's `invokestatic` with `argL1` as its
//!   first argument. No guard: the VM admission found the bound value to be a
//!   non-null instance of the parameter's class, the field is `final`, and a
//!   static callee takes any value of its parameter type anyway, so nothing
//!   about the call can differ from the handle's own (`invokeBasic(argL0,
//!   argL1, a*)` of a static direct handle is `linkToStatic(argL1, a*,
//!   member)`). The class of the member is initialised by the `invokestatic`
//!   arm's own rule, as the direct handle's `ensureInitialized` would.
//!
//! The plan's kind and the row the planner baked must agree; any other pairing
//! (a static plan over an instance row, a virtual plan over a static row) is
//! refused, so a bound value is never passed where the other shape expects it.

/// What the bound site at hand builds: `(opcode, receiver null guard)`, or
/// `None` to refuse the method. `invoke_kind` is the baked row's
/// (`JitInvokeInfo::invoke_kind`: `3` static, `0` / `1` instance), `planned_static`
/// whether the plan named the site a bound STATIC one, `num_args` the row's
/// argument count (the bound value counts: it stands where the handle was),
/// `interface` whether the plan named it an interface site.
pub(crate) fn bound_site_build(
    invoke_kind: u8,
    planned_static: bool,
    num_args: usize,
    interface: bool,
) -> Option<(u8, bool)> {
    if num_args == 0 || interface {
        return None;
    }
    match (invoke_kind, planned_static) {
        (3, true) => Some((0xb8, false)),
        (0 | 1, false) => Some((0xb6, true)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
