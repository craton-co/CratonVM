// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The CALL-SITE descriptor of a signature-polymorphic invocation, for the one
//! native that cannot do its job without it.
//!
//! # Why this exists
//!
//! `MethodHandle.invoke` is compiled to `asType(callSiteType)` followed by an
//! exact invocation, and `asType` is where a varargs collector decides whether
//! to WRAP the trailing argument or pass it straight through: the shortcut is
//! taken only when the caller's trailing parameter type is assignable to the
//! collector's array type. So the same handle, the same runtime value, and two
//! different answers:
//!
//! ```text
//!   mh.invoke((String[]) null)   ->  passthrough  ->  Arrays.toString(null)     "null"
//!   mh.invoke((Object)   null)   ->  collect      ->  Arrays.toString({null})   "[null]"
//! ```
//!
//! A `NativeMethodRegistry` callback receives only `&[Value]`, and a `null`
//! carries no runtime type, so the `invoke` shim had nothing to decide on and
//! answered `"null"` for both. `vm_exec`'s signature-polymorphic dispatch block
//! HAS the descriptor — it is the same local it already hands
//! `unbox_poly_return_checked` — and this is the channel from there to here.
//!
//! # Contract
//!
//! [`arm`] is called immediately before the native call, and the consuming
//! native TAKES it. Take, not peek: a value left armed would be read by a later
//! dispatch through a door that never armed one, and a WRONG call-site type is
//! worse than none. For the same reason the dispatch block clears it for every
//! signature-polymorphic name it does not arm, so a `DowncallHandle.invoke`
//! that never consumes cannot leave one behind.
//!
//! A miss degrades to the pre-2026-08-21 behaviour (dispatch decides from the
//! runtime argument shape alone), never to a wrong answer, which is what makes
//! it safe that the JIT's own signature-polymorphic fast path
//! (`resolve_signature_polymorphic_native_site`) admits VarHandle receivers
//! only and so never arms.

use std::cell::RefCell;

thread_local! {
    static POLY_CALL_SITE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Publish the call-site descriptor for the native about to run on this thread.
pub fn arm(descriptor: &str) {
    POLY_CALL_SITE.with(|c| *c.borrow_mut() = Some(descriptor.to_string()));
}

/// Drop any armed descriptor. Called for every signature-polymorphic name the
/// dispatcher does NOT arm, so a stale one can never be read as this call's.
pub fn clear() {
    POLY_CALL_SITE.with(|c| *c.borrow_mut() = None);
}

/// Read and clear the armed descriptor.
pub fn take() -> Option<String> {
    POLY_CALL_SITE.with(|c| c.borrow_mut().take())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TAKEN, not peeked — the property the whole channel rests on.
    #[test]
    fn the_call_site_descriptor_is_taken_once() {
        assert_eq!(take(), None, "a fresh thread has nothing armed");
        arm("(Ljava/lang/Object;)Ljava/lang/String;");
        assert_eq!(
            take().as_deref(),
            Some("(Ljava/lang/Object;)Ljava/lang/String;")
        );
        assert_eq!(take(), None, "a second reader must not see the first's");
        arm("(I)V");
        clear();
        assert_eq!(take(), None, "clear discards an armed descriptor");
    }
}
