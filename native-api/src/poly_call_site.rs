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

use std::cell::{Cell, RefCell};

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

// ---------------------------------------------------------------------------
// The VarHandle VALUE-site channel
// ---------------------------------------------------------------------------
//
// A `VarHandle` access is `asType(callSiteType)` followed by the access, and
// for a PRIMITIVE variable `asType` decides by the STATIC type of each value
// parameter at the call site (`MethodType.canConvert`, then
// `ValueConversions.unboxWiden`):
//
// ```text
//   int field, site value type    value            HotSpot 25
//   Object / Number / Comparable  Integer(5)       stores 5
//   Object                        Short(5)         stores 5 (widened)
//   Object                        Long(5)          ClassCastException
//   Object                        "nine"           ClassCastException
//   Object                        null             NullPointerException
//   String                        (anything)       WrongMethodTypeException
//   Long                          (anything)       WrongMethodTypeException
//   int                           5                stores 5
//   int  (into a float field)     5                stores 5.0f
// ```
//
// The native sees only `&[Value]`, and `"nine"` arrives identically from an
// `Object` site and a `String` site, so the native cannot tell the CCE rows
// from the WMTE rows without this. It is a SEPARATE cell from
// `POLY_CALL_SITE` for two reasons, both from
// `docs/internal/fixed-bugs/r11w15-rt-varhandle-call-site-descriptor-patch-FIXED-20260925.md`:
// arming `POLY_CALL_SITE` turns the native-callee memo off for the name, and
// it allocates a `String` per dispatch on the CAS-hot path. This cell is two
// bytes, and the classification is a pure function of the descriptor.
//
// Same TAKE contract as the descriptor channel: the consuming native takes it
// at its very top, before any early return, so it cannot outlive the call it
// was armed for. A door that did not arm leaves `0` = unknown, and the
// consumer then applies the `Object`-site rule (the common way a reference
// reaches a primitive-typed VarHandle: generic code holding the value as
// `Object`).

/// Static-type classes of one VarHandle VALUE parameter, as published by
/// [`vh_value_site_classes`]. A PRIMITIVE parameter is published as its own
/// descriptor byte (`b'I'`, `b'J'`, ...), which cannot collide with these.
pub mod vh_site {
    /// Nothing armed: the door did not publish (the consumer assumes `Object`).
    pub const UNKNOWN: u8 = 0;
    /// `java/lang/Object`.
    pub const OBJECT: u8 = 1;
    /// `java/lang/Number`.
    pub const NUMBER: u8 = 2;
    /// `java/io/Serializable`.
    pub const SERIALIZABLE: u8 = 3;
    /// `java/lang/Comparable`.
    pub const COMPARABLE: u8 = 4;
    /// `java/lang/constant/Constable`.
    pub const CONSTABLE: u8 = 5;
    /// `java/lang/constant/ConstantDesc`.
    pub const CONSTANT_DESC: u8 = 6;
    /// `java/lang/Boolean`.
    pub const W_BOOLEAN: u8 = 7;
    /// `java/lang/Byte`.
    pub const W_BYTE: u8 = 8;
    /// `java/lang/Character`.
    pub const W_CHARACTER: u8 = 9;
    /// `java/lang/Short`.
    pub const W_SHORT: u8 = 10;
    /// `java/lang/Integer`.
    pub const W_INTEGER: u8 = 11;
    /// `java/lang/Long`.
    pub const W_LONG: u8 = 12;
    /// `java/lang/Float`.
    pub const W_FLOAT: u8 = 13;
    /// `java/lang/Double`.
    pub const W_DOUBLE: u8 = 14;
    /// Any other class or interface, and every array type.
    pub const OTHER: u8 = 15;

    /// The primitive descriptor byte a WRAPPER class code stands for.
    pub fn wrapper_prim(class: u8) -> Option<u8> {
        Some(match class {
            W_BOOLEAN => b'Z',
            W_BYTE => b'B',
            W_CHARACTER => b'C',
            W_SHORT => b'S',
            W_INTEGER => b'I',
            W_LONG => b'J',
            W_FLOAT => b'F',
            W_DOUBLE => b'D',
            _ => return None,
        })
    }

    /// Is `class` a PRIMITIVE parameter (published as its descriptor byte)?
    pub fn is_primitive(class: u8) -> bool {
        matches!(
            class,
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D'
        )
    }
}

/// Classify ONE parameter descriptor token (`I`, `Ljava/lang/Object;`,
/// `[I`, ...) into a [`vh_site`] class.
///
/// `cratonvm_jit`'s `varhandle_reference_value_kind` splits bound VarHandle
/// sites by the same groups (it cannot call this; keep the two in step), and
/// its `R` slots' synthetic call sites name `java/lang/Void` precisely because
/// it lands in `OTHER` here (round 11 wave 18). Do not give `Void` a class of
/// its own.
fn vh_param_class(token: &str) -> u8 {
    let b = token.as_bytes();
    if b.len() == 1 && vh_site::is_primitive(b[0]) {
        return b[0];
    }
    match token {
        "Ljava/lang/Object;" => vh_site::OBJECT,
        "Ljava/lang/Number;" => vh_site::NUMBER,
        "Ljava/io/Serializable;" => vh_site::SERIALIZABLE,
        "Ljava/lang/Comparable;" => vh_site::COMPARABLE,
        "Ljava/lang/constant/Constable;" => vh_site::CONSTABLE,
        "Ljava/lang/constant/ConstantDesc;" => vh_site::CONSTANT_DESC,
        "Ljava/lang/Boolean;" => vh_site::W_BOOLEAN,
        "Ljava/lang/Byte;" => vh_site::W_BYTE,
        "Ljava/lang/Character;" => vh_site::W_CHARACTER,
        "Ljava/lang/Short;" => vh_site::W_SHORT,
        "Ljava/lang/Integer;" => vh_site::W_INTEGER,
        "Ljava/lang/Long;" => vh_site::W_LONG,
        "Ljava/lang/Float;" => vh_site::W_FLOAT,
        "Ljava/lang/Double;" => vh_site::W_DOUBLE,
        _ => vh_site::OTHER,
    }
}

/// The [`vh_site`] classes of the LAST TWO parameters of a call-site
/// descriptor, packed as `(second_to_last << 8) | last`, or `0` when the
/// descriptor does not parse or has no parameters.
///
/// The value parameters of every VarHandle access mode TRAIL its coordinates:
/// `set*`/`getAndSet*`/`getAndAdd*`/`getAndBitwise*` have one (the last),
/// `compareAnd*`/`weakCompareAndSet*` have two (expected, then new). So the
/// classifier needs neither the method name nor the coordinate count, and the
/// consumer, which knows its own access mode, reads the byte(s) it owns.
pub fn vh_value_site_classes(descriptor: &str) -> u16 {
    let b = descriptor.as_bytes();
    if b.first() != Some(&b'(') {
        return 0;
    }
    let mut i = 1;
    let mut last: Option<(usize, usize)> = None;
    let mut prev: Option<(usize, usize)> = None;
    while i < b.len() && b[i] != b')' {
        let start = i;
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        match b.get(i) {
            Some(b'L') => match descriptor[i..].find(';') {
                Some(end) => i += end + 1,
                None => return 0,
            },
            Some(c) if vh_site::is_primitive(*c) => i += 1,
            _ => return 0,
        }
        prev = last;
        last = Some((start, i));
    }
    if b.get(i) != Some(&b')') {
        return 0;
    }
    let class_of = |r: Option<(usize, usize)>| -> u16 {
        r.map_or(0, |(s, e)| vh_param_class(&descriptor[s..e]) as u16)
    };
    (class_of(prev) << 8) | class_of(last)
}

thread_local! {
    static VH_VALUE_SITE: Cell<u16> = const { Cell::new(0) };
}

/// Publish the value-site classes ([`vh_value_site_classes`]) for the
/// VarHandle store-mode native about to run on this thread.
pub fn arm_vh_value_site(classes: u16) {
    VH_VALUE_SITE.with(|c| c.set(classes));
}

/// Drop an armed value-site classification that no native consumed.
pub fn clear_vh_value_site() {
    VH_VALUE_SITE.with(|c| c.set(0));
}

/// Read and clear the armed value-site classes; `0` when nothing was armed.
pub fn take_vh_value_site() -> u16 {
    VH_VALUE_SITE.with(|c| c.replace(0))
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

    /// The probe `R11W15RtVarHandleObjectSite`'s three call-site shapes, and
    /// the two-value `compareAndSet` shape: the classes land in the right
    /// byte, and a primitive is published as its own descriptor byte.
    #[test]
    fn vh_value_site_classes_reads_the_trailing_parameters() {
        let pack = |prev: u8, last: u8| ((prev as u16) << 8) | last as u16;
        assert_eq!(
            vh_value_site_classes("(LR11$H;Ljava/lang/Object;)V"),
            pack(vh_site::OTHER, vh_site::OBJECT)
        );
        assert_eq!(
            vh_value_site_classes("(LR11$H;Ljava/lang/String;)V"),
            pack(vh_site::OTHER, vh_site::OTHER)
        );
        assert_eq!(
            vh_value_site_classes("(LR11$H;I)V"),
            pack(vh_site::OTHER, b'I')
        );
        assert_eq!(
            vh_value_site_classes("(LR11$H;Ljava/lang/Long;Ljava/lang/Number;)Z"),
            pack(vh_site::W_LONG, vh_site::NUMBER)
        );
        // A static handle's `set`: one parameter, nothing before it.
        assert_eq!(vh_value_site_classes("(J)V"), pack(0, b'J'));
        // Array-element coordinates, and array-typed values, parse.
        assert_eq!(
            vh_value_site_classes("([IILjava/lang/Integer;)V"),
            pack(b'I', vh_site::W_INTEGER)
        );
        assert_eq!(
            vh_value_site_classes("(Ljava/lang/Object;[[Ljava/lang/String;)V"),
            pack(vh_site::OBJECT, vh_site::OTHER)
        );
        // The JIT's `R`-slot stand-ins (round 11 wave 18) must read as OTHER.
        assert_eq!(
            vh_value_site_classes("(Ljava/lang/Object;Ljava/lang/Void;Ljava/lang/Void;)Z"),
            pack(vh_site::OTHER, vh_site::OTHER)
        );
        // Nothing to classify, or nothing parseable: unknown.
        assert_eq!(vh_value_site_classes("()V"), 0);
        assert_eq!(vh_value_site_classes("(Ljava/lang/Object"), 0);
        assert_eq!(vh_value_site_classes("I)V"), 0);
    }

    /// Taken once, cleared by `clear_vh_value_site`, and independent of the
    /// descriptor channel above.
    #[test]
    fn the_vh_value_site_is_taken_once_and_separate() {
        assert_eq!(take_vh_value_site(), 0, "a fresh thread has nothing armed");
        arm_vh_value_site(0x0f01);
        assert_eq!(take(), None, "the descriptor channel is a different cell");
        assert_eq!(take_vh_value_site(), 0x0f01);
        assert_eq!(take_vh_value_site(), 0, "a second reader must not see it");
        arm_vh_value_site(7);
        clear_vh_value_site();
        assert_eq!(take_vh_value_site(), 0);
        assert_eq!(vh_site::wrapper_prim(vh_site::W_CHARACTER), Some(b'C'));
        assert_eq!(vh_site::wrapper_prim(vh_site::OBJECT), None);
        assert!(vh_site::is_primitive(b'Z') && !vh_site::is_primitive(vh_site::OTHER));
    }
}
