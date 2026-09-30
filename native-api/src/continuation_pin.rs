// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What a virtual thread's continuation may be unwound through, on the thread
//! running it (interpreter round i1 wave 26, lane L4; page
//! `docs/internal/fixed-bugs/interpreter-L4-a-virtual-thread-that-unmounts-under-a-native-loses-the-natives-return-conversion-FIXED-20260928.md`).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_guard_releases_what_it_took() {
        assert_eq!((pins(), transparent_natives()), (0, 0));
        {
            let _p = pin();
            let _t = transparent();
            let _t2 = transparent();
            assert_eq!((pins(), transparent_natives()), (1, 2));
        }
        assert_eq!((pins(), transparent_natives()), (0, 0));
    }
}
