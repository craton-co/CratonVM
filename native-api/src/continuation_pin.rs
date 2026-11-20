// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! What a virtual thread's continuation may be unwound through, on the thread
//! running it (interpreter round i1 wave 26, lane L4; page
//! `docs/feature-designs/native-dispatch-architecture.md`).
//!
//! A continuation is the thread's Java frames only: a yield unwinds every Rust
//! frame between the yielding native and the carrier, and the remount runs the
//! Java frames alone. The VM's yield decisions (`vm_exec`'s `vt_park_for` and
//! siblings, through `native_callee_memo::continuation_pinned_by_native`)
//! therefore refuse to yield while a Rust frame with post-call work is on the
//! stack: a native that re-entered Java (the native funnel's depth), or one of
//! the two counts kept here, which live in this crate because both the VM and
//! the natives (`native-builtins`) raise them.
//!
//! * [`pin`]: a non-native Rust door that runs Java in a nested call and still
//!   owes its caller something when the call returns (the lambda door's
//!   constructor reference owes the new object; the proxy door owes the
//!   handler result's conversion). Counted UP: it pins.
//! * [`transparent`]: a native whose post-call work is the identity for this
//!   one nested call -- a `MethodHandle` door whose leaf returns a reference
//!   the door hands back unchanged, through adapters that do nothing after the
//!   call. HotSpot has no native frame there at all (method handles are Java),
//!   so the continuation may unmount; counted DOWN from the funnel's depth.
//!
//! Both are per-thread and scoped by an RAII guard, so they are transient
//! dispatch state (like `poly_call_site`), not per-VM state. A yield unwinds
//! through the guards' `Drop`, which restores the counts on the old carrier.

use std::cell::Cell;

thread_local! {
    static PINS: Cell<u32> = const { Cell::new(0) };
    static TRANSPARENT: Cell<u32> = const { Cell::new(0) };
}

/// One pin taken by [`pin`]; released on drop.
#[must_use = "the pin is released when the guard drops"]
pub struct PinGuard(());

impl Drop for PinGuard {
    #[inline]
    fn drop(&mut self) {
        PINS.with(|p| p.set(p.get().saturating_sub(1)));
    }
}

/// Pin this thread's continuation until the guard drops: a virtual thread that
/// parks or sleeps meanwhile blocks its carrier instead of unmounting.
#[inline]
pub fn pin() -> PinGuard {
    PINS.with(|p| p.set(p.get().wrapping_add(1)));
    PinGuard(())
}

/// Pins held on this thread ([`pin`]).
#[inline]
pub fn pins() -> u32 {
    PINS.with(Cell::get)
}

/// One transparent native frame declared by [`transparent`]; withdrawn on drop.
#[must_use = "the declaration is withdrawn when the guard drops"]
pub struct TransparentGuard(());

impl Drop for TransparentGuard {
    #[inline]
    fn drop(&mut self) {
        TRANSPARENT.with(|t| t.set(t.get().saturating_sub(1)));
    }
}

/// Declare that the innermost running native (the caller) does nothing after
/// the nested Java call it is about to make but hand its result back
/// unchanged, until the guard drops: its frame may be unwound by a yield.
#[inline]
pub fn transparent() -> TransparentGuard {
    TRANSPARENT.with(|t| t.set(t.get().wrapping_add(1)));
    TransparentGuard(())
}

/// Transparent native frames on this thread ([`transparent`]).
#[inline]
pub fn transparent_natives() -> u32 {
    TRANSPARENT.with(Cell::get)
}

/// Replace this thread's two counts with `(pins, transparent)` and answer the
/// previous pair (interpreter round i6 wave 130, lane L6). For a JDK
/// continuation slice run on its CARRIER's own OS thread
/// (`CRATONVM_VTHREAD_SLICE_ON_CARRIER`): the slice starts from `(0, 0)`, as
/// on a fresh runner, and the carrier's pair comes back when it ends. Every
/// guard taken inside the slice is dropped by then (a yield unwinds through
/// them), so the swap never strands a count.
#[inline]
pub fn replace_counts(pins: u32, transparent: u32) -> (u32, u32) {
    (
        PINS.with(|p| p.replace(pins)),
        TRANSPARENT.with(|t| t.replace(transparent)),
    )
}

/// Run `call` -- a registered native's TAIL call into Java -- with the
/// native's frame declared [`transparent`] for it (gci i18/vp, page
/// `gci-i15orch-virtual-threads-never-start-under-nojit-so-300-thread-probes-hang-FIXED-20261015`).
///
/// The contract a call site states by using this (each site's comment says
/// why it holds there):
///
/// * the native is the innermost registered native on this thread, reached
///   through the counted native funnel (not a leaf registration, not a Rust
///   helper called from another native's body with work of its own after
///   the call), so the count this guard takes away is its own frame;
/// * the nested call's answer -- a value, an exception, or a continuation
///   yield -- is what the native itself answers, unchanged, and the nested
///   method returns the same descriptor kind as the native's own method (a
///   remounted continuation hands the nested frame's return straight to the
///   native's caller and never runs the native's code after the call);
/// * anything the native does after the call is the identity for its caller
///   (refreshing a cache or a method-index hint, releasing a native pin taken
///   for the call itself).
///
/// That is the frame HotSpot does not have: these natives stand in front of
/// a JDK (or application) body that HotSpot runs as plain Java, so a virtual
/// thread that parks inside it unmounts there. Counted as an opaque native,
/// the park pinned (`continuation_pinned_by_native`) and held its carrier.
#[inline]
pub fn tail_call<R>(call: impl FnOnce() -> R) -> R {
    let _transparent = transparent();
    call()
}

#[cfg(test)]
mod tests;
