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
use std::sync::Arc;

/// How many recently armed descriptors [`arm`] keeps for reuse, per thread.
const RECENT_SITES: usize = 4;

/// The descriptor channel's per-thread state: the armed descriptor, and the
/// last [`RECENT_SITES`] distinct descriptors armed through [`arm`], which it
/// hands out again (a refcount) instead of allocating a copy per call.
///
/// Two fixes for the same per-call allocation meet here, and both hold:
///
/// * Round 15 wave 4 (lane mh2; MH15-2 of `jit-r15-mh-proposals-RETIRED-20261003.md`): a
///   compiled `invoke` / `invokeExact` arms on EVERY call through [`arm`] (the
///   JIT's carrier site, `vm/src/jit/helpers.rs`), and a `String` per arm was a
///   malloc/free pair on the direct lane's ~2 us floor. A loop calls the same
///   site, or a handful of them, so a short ring of shared descriptors answers
///   it with a compare (`CRATONVM_MH_CALL_SITE_REUSE`, default on).
/// * Interpreter round i1 wave 49, lane L7 (proposal
///   `interpreter-L7-proposal-the-call-site-descriptor-channel-allocates-per-call-FIXED-20261013`):
///   a door that already owns the site's descriptor as an `Arc<str>` (the
///   interpreter's signature-polymorphic site cache, `SigPolySite::descriptor`)
///   arms it with a reference-count bump ([`arm_shared`]); that path bypasses
///   the ring, since the door's own allocation is already the shared copy.
///
/// The shared text is `Arc<str>` (the type the interpreter's site cache holds)
/// and immutable, so a reader can never see another call's descriptor through
/// it: the TAKE contract is the `armed` slot's, unchanged.
struct PolyCallSite {
    armed: Option<Arc<str>>,
    /// The class id of the CALLER the arming door names with the descriptor
    /// ([`arm_with_caller`]), `0` when the door named none. Reset by every
    /// arm, moved to `taken_caller` by [`take`].
    armed_caller: u32,
    /// The caller published with the descriptor the last [`take`] on this
    /// thread returned, `0` when that arm named none ([`taken_caller`]).
    taken_caller: u32,
    recent: [Option<Arc<str>>; RECENT_SITES],
    next: usize,
    /// `CRATONVM_MH_CALL_SITE_REUSE` for this thread: `0` unread, `1` off,
    /// `2` on.
    reuse: u8,
}

impl PolyCallSite {
    /// `descriptor` as a shared string: a recent one when equal, else a new
    /// one (remembered in the ring when reuse is on).
    fn shared(&mut self, descriptor: &str) -> Arc<str> {
        if self.reuse == 0 {
            // Read once per thread; nothing on the arm path runs Java.
            self.reuse =
                if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_CALL_SITE_REUSE") {
                    2
                } else {
                    1
                };
        }
        if self.reuse != 2 {
            return Arc::from(descriptor);
        }
        if let Some(hit) = self.recent.iter().flatten().find(|s| &***s == descriptor) {
            return Arc::clone(hit);
        }
        let fresh: Arc<str> = Arc::from(descriptor);
        let slot = self.next % RECENT_SITES;
        self.recent[slot] = Some(Arc::clone(&fresh));
        self.next = (slot + 1) % RECENT_SITES;
        fresh
    }
}

// A `RefCell` (not the plain `Cell` the L7 change used on its own): the ring
// is read and updated in place on every [`arm`]. Nothing on these paths runs
// Java or re-enters the channel, so the borrow is never contended; the only
// code that runs while a borrow is held is an `Arc` drop.
thread_local! {
    static POLY_CALL_SITE: RefCell<PolyCallSite> = const {
        RefCell::new(PolyCallSite {
            armed: None,
            armed_caller: 0,
            taken_caller: 0,
            recent: [None, None, None, None],
            next: 0,
            reuse: 0,
        })
    };
}

/// Publish the call-site descriptor for the native about to run on this thread.
/// Reuses a recently armed shared copy when equal (MH15-2), else allocates
/// one; a door that holds one already uses [`arm_shared`].
pub fn arm(descriptor: &str) {
    POLY_CALL_SITE.with(|c| {
        let mut c = c.borrow_mut();
        let shared = c.shared(descriptor);
        c.armed = Some(shared);
        c.armed_caller = 0;
    });
}

/// [`arm`], also naming the class id of the method that holds the call site
/// (`0` = unknown), for a consumer that would otherwise walk the stack for
/// its caller ([`taken_caller`]). Interpreter round i5 wave 107, lane L4
/// (`CRATONVM_MH_ARMED_CALLER`, default ON on the consumer's side since the w107 A/B): the
/// JIT's carrier door passes its site's `JitInvokeInfo::declaring_class_id`,
/// the class whose constant pool the compiled site resolves through.
pub fn arm_with_caller(descriptor: &str, caller_class_id: u32) {
    POLY_CALL_SITE.with(|c| {
        let mut c = c.borrow_mut();
        let shared = c.shared(descriptor);
        c.armed = Some(shared);
        c.armed_caller = caller_class_id;
    });
}

/// [`arm`] with a descriptor the door already holds as an `Arc<str>`: a
/// reference-count bump, no allocation. The consumer reads exactly the same
/// text either way.
pub fn arm_shared(descriptor: &Arc<str>) {
    POLY_CALL_SITE.with(|c| {
        let mut c = c.borrow_mut();
        c.armed = Some(Arc::clone(descriptor));
        c.armed_caller = 0;
    });
}

/// Drop any armed descriptor. Called for every signature-polymorphic name the
/// dispatcher does NOT arm, so a stale one can never be read as this call's.
pub fn clear() {
    POLY_CALL_SITE.with(|c| {
        let mut c = c.borrow_mut();
        c.armed = None;
        c.armed_caller = 0;
    });
}

/// Read and clear the armed descriptor. A shared, immutable string (the
/// per-thread ring of recent descriptors, MH15-2, or the door's own
/// allocation via [`arm_shared`]); a consumer that must own one copies it
/// (`as_deref().map(str::to_owned)`).
pub fn take() -> Option<Arc<str>> {
    POLY_CALL_SITE.with(|c| {
        let mut c = c.borrow_mut();
        let caller = std::mem::take(&mut c.armed_caller);
        c.taken_caller = caller;
        c.armed.take()
    })
}

/// The caller class id armed with the descriptor the last [`take`] on this
/// thread returned ([`arm_with_caller`]), `0` when that arm named none or
/// nothing was armed. Read it right after [`take`], before anything that can
/// re-enter Java (a nested signature-polymorphic call takes again). A HINT:
/// a consumer must answer exactly what its own caller walk would answer
/// whenever it uses it (see `lang_invoke::real_lane_site_admits`).
pub fn taken_caller() -> u32 {
    POLY_CALL_SITE.with(|c| c.borrow().taken_caller)
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
// `docs/feature-designs/profile-guided-inlining.md`:
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
        matches!(class, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
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
///
/// The low half of [`vh_param_site_classes`], by construction.
pub fn vh_value_site_classes(descriptor: &str) -> u16 {
    // Cast: the low 16 bits are the last two parameters' classes.
    (vh_param_site_classes(descriptor) & 0xffff) as u16
}

/// How many trailing call-site parameters [`vh_param_site_classes`] covers.
/// Seven: the word's top byte is the call site's return
/// ([`VH_SITE_RETURN_SHIFT`], interpreter round i2 wave 57, lane L4).
pub const VH_PARAM_SITE_SLOTS: usize = 7;

/// Where [`vh_param_site_word`] puts the call site's return: a primitive's
/// descriptor byte, a [`vh_site`] class code for a reference other than
/// `Object`, `0` for `Object` or `void` (and in a word from
/// [`vh_param_site_classes`]).
pub const VH_SITE_RETURN_SHIFT: u32 = 56;

/// The parameter bytes of a [`vh_param_site_word`] (the return byte masked
/// off), for a reader asking "are there classes above the value bytes".
pub const VH_PARAM_SITE_PARAM_MASK: u64 = (1u64 << VH_SITE_RETURN_SHIFT) - 1;

/// [`vh_param_site_classes`] plus, in the top byte, the call site's return
/// ([`VH_SITE_RETURN_SHIFT`]): what the access-mode natives judge the
/// variable's type against, as `asType` does on HotSpot (`(int) vh.get(o)`
/// over a `long` field, `(String) vh.get(o)` over an `int` one, are a
/// `WrongMethodTypeException`). Interpreter round i2 wave 57, lane L4
/// (`interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021`).
/// A pure function of the descriptor, computed where the doors compute the
/// word today (once per site on the cached doors).
pub fn vh_param_site_word(descriptor: &str) -> u64 {
    let classes = vh_param_site_classes(descriptor);
    let Some(token) = descriptor.rsplit(')').next() else {
        return classes;
    };
    if !descriptor.starts_with('(') || token.is_empty() || token.len() == descriptor.len() {
        return classes;
    }
    let ret = match vh_param_class(token) {
        // `void`, and `Object` (which every variable converts to).
        vh_site::OBJECT => 0,
        _ if token == "V" => 0,
        // A lone byte that is not a descriptor letter: not judged.
        c if token.len() == 1 && !vh_site::is_primitive(c) => 0,
        // An object type that is not `L...;` or `[...`: not judged.
        _ if token.len() > 1
            && !(token.starts_with('L') && token.ends_with(';'))
            && !token.starts_with('[') =>
        {
            0
        }
        c => c,
    };
    classes | (u64::from(ret) << VH_SITE_RETURN_SHIFT)
}

/// The return byte of a [`vh_param_site_word`]: a primitive's descriptor
/// byte, a [`vh_site`] class code (`2..=15`, below every descriptor letter)
/// for a reference other than `Object`, or `0` when the site returns
/// `Object` or `void`, or nothing was armed.
#[inline]
pub fn vh_param_site_return(site: u64) -> u8 {
    // Cast: the top byte.
    (site >> VH_SITE_RETURN_SHIFT) as u8
}

/// The [`vh_site`] classes of the LAST [`VH_PARAM_SITE_SLOTS`] parameters of a
/// call-site descriptor, one byte each, the LAST parameter in the low byte
/// (so the low 16 bits are [`vh_value_site_classes`]); `0` when the
/// descriptor does not parse or has no parameters. A byte of `0` is a
/// parameter the word does not cover.
///
/// Round 15 wave 3 (lane vh3; VH1-3 in `jit-r15-vh-proposals-RETIRED-20261003.md`): the
/// COORDINATES precede the values, and `asType` judges them too (`int` into a
/// `char` coordinate is a `WrongMethodTypeException`), so a consumer that
/// knows its declared parameter list reads every covered position, not only
/// the two values. A pure function of the descriptor: no `String` per access.
pub fn vh_param_site_classes(descriptor: &str) -> u64 {
    let b = descriptor.as_bytes();
    if b.first() != Some(&b'(') {
        return 0;
    }
    // The last `VH_PARAM_SITE_SLOTS` parameter spans, as a ring.
    let mut ring = [(0usize, 0usize); VH_PARAM_SITE_SLOTS];
    let mut count = 0usize;
    let mut i = 1;
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
        ring[count % VH_PARAM_SITE_SLOTS] = (start, i);
        count += 1;
    }
    if b.get(i) != Some(&b')') {
        return 0;
    }
    let mut packed = 0u64;
    for k in 0..count.min(VH_PARAM_SITE_SLOTS) {
        let (s, e) = ring[(count - 1 - k) % VH_PARAM_SITE_SLOTS];
        packed |= u64::from(vh_param_class(&descriptor[s..e])) << (8 * k);
    }
    packed
}

/// The [`vh_site`] class of the call-site parameter `back` places before the
/// last one (`0` = the last) in a [`vh_param_site_classes`] word; `UNKNOWN`
/// beyond the covered positions.
pub fn vh_param_site_class_at(site: u64, back: usize) -> u8 {
    if back >= VH_PARAM_SITE_SLOTS {
        return vh_site::UNKNOWN;
    }
    // Cast: one byte of the packed word.
    ((site >> (8 * back)) & 0xff) as u8
}

thread_local! {
    // The whole [`vh_param_site_classes`] word; a door that arms only the
    // value classes ([`arm_vh_value_site`]) leaves the upper bytes `0`.
    static VH_VALUE_SITE: Cell<u64> = const { Cell::new(0) };
}

/// Publish the value-site classes ([`vh_value_site_classes`]) for the
/// VarHandle store-mode native about to run on this thread. The coordinates'
/// bytes read `UNKNOWN` (not judged).
pub fn arm_vh_value_site(classes: u16) {
    VH_VALUE_SITE.with(|c| c.set(u64::from(classes)));
}

/// Publish the full parameter-site word ([`vh_param_site_classes`]) for the
/// VarHandle access-mode native about to run on this thread (round 15 wave 3,
/// lane vh3). Same cell and same TAKE contract as [`arm_vh_value_site`]; a
/// read mode's native takes it too.
pub fn arm_vh_param_site(classes: u64) {
    VH_VALUE_SITE.with(|c| c.set(classes));
}

/// Drop an armed value-site classification that no native consumed.
pub fn clear_vh_value_site() {
    VH_VALUE_SITE.with(|c| c.set(0));
}

/// Read and clear the armed value-site classes (the low half of the cell);
/// `0` when nothing was armed.
pub fn take_vh_value_site() -> u16 {
    // Cast: the low 16 bits are the value-site classes.
    (take_vh_param_site() & 0xffff) as u16
}

/// Read and clear the whole armed parameter-site word; `0` when nothing was
/// armed.
pub fn take_vh_param_site() -> u64 {
    VH_VALUE_SITE.with(|c| c.replace(0))
}

// ---------------------------------------------------------------------------
// The VarHandle `asType` refusal record (interpreter round i2 wave 53, lane L4)
// ---------------------------------------------------------------------------
//
// HotSpot's `WrongMethodTypeException` for a VarHandle parameter the call
// site's static type cannot convert is the invoker's `asType` refusal, and it
// names BOTH method types:
//
// ```text
//   cannot convert MethodHandle(VarHandle,int[],int,int)int to (VarHandle,int[],int,double)Object
// ```
//
// The native that refuses knows the handle's own type but not the call
// site's (the doors arm only the class word above, never the descriptor, on
// the CAS-hot path); the door knows the site's descriptor and the method name
// but must not read the handle after the native ran (it may have moved). So
// the refusing native leaves this record -- cold, only on a refusal -- and the
// door that armed the site word takes it on its error path and rewrites the
// message. The record names the exception it belongs to: a door that never
// takes it (an unarmed dispatch) leaves one behind, and a later door then
// sees a different exception and discards it.

/// What a refusing VarHandle native knew about its handle: the coordinate
/// descriptors and the variable's descriptor (the access mode type follows
/// from them and the method name), and the exception it raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VhAsTypeRefusal {
    /// The `WrongMethodTypeException` the native returned.
    pub exception: cratonvm_types::ObjectRef,
    /// The handle's coordinate descriptors, in order.
    pub coordinates: Vec<String>,
    /// The handle's variable descriptor.
    pub value: String,
}

thread_local! {
    static VH_ASTYPE_REFUSAL: RefCell<Option<VhAsTypeRefusal>> = const { RefCell::new(None) };
}

/// Leave the refusal record for the door (see the block comment above).
pub fn note_vh_astype_refusal(refusal: VhAsTypeRefusal) {
    VH_ASTYPE_REFUSAL.with(|c| *c.borrow_mut() = Some(refusal));
}

/// Take the refusal record if it belongs to `exception`; any other record is
/// dropped (it belonged to a dispatch no door took it from).
pub fn take_vh_astype_refusal(exception: cratonvm_types::ObjectRef) -> Option<VhAsTypeRefusal> {
    VH_ASTYPE_REFUSAL
        .with(|c| c.borrow_mut().take())
        .filter(|r| r.exception == exception)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wave 53, lane L4: the refusal record is taken only by the door whose
    /// exception it names, and any take drops it.
    #[test]
    fn the_astype_refusal_record_is_taken_by_its_own_exception_only() {
        // SAFETY: never dereferenced; non-null and 8-aligned, as `from_raw`
        // requires (the record compares identities only).
        let exc = |bits: usize| unsafe { cratonvm_types::ObjectRef::from_raw(bits as *mut u8) };
        let record = |e| VhAsTypeRefusal {
            exception: e,
            coordinates: vec!["[I".to_string(), "I".to_string()],
            value: "I".to_string(),
        };
        assert_eq!(take_vh_astype_refusal(exc(0x1000)), None, "nothing noted");
        note_vh_astype_refusal(record(exc(0x1000)));
        assert_eq!(
            take_vh_astype_refusal(exc(0x1000)),
            Some(record(exc(0x1000)))
        );
        assert_eq!(take_vh_astype_refusal(exc(0x1000)), None, "taken once");
        note_vh_astype_refusal(record(exc(0x2000)));
        assert_eq!(
            take_vh_astype_refusal(exc(0x1000)),
            None,
            "another exception's"
        );
        assert_eq!(
            take_vh_astype_refusal(exc(0x2000)),
            None,
            "dropped by that take"
        );
    }

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

    /// Interpreter round i5 wave 107, lane L4: the caller travels with the
    /// descriptor it was armed with and with no other one.
    #[test]
    fn the_armed_caller_belongs_to_the_take_it_was_armed_for() {
        assert_eq!(taken_caller(), 0, "a fresh thread has none");
        arm_with_caller("(LBox;)I", 42);
        assert_eq!(take().as_deref(), Some("(LBox;)I"));
        assert_eq!(taken_caller(), 42);
        assert_eq!(taken_caller(), 42, "a read does not consume it");
        arm("(LBox;)I");
        assert_eq!(take().as_deref(), Some("(LBox;)I"));
        assert_eq!(taken_caller(), 0, "a plain arm names no caller");
        arm_with_caller("(I)V", 7);
        clear();
        assert_eq!(take(), None);
        assert_eq!(taken_caller(), 0, "clear drops the caller too");
        arm_with_caller("(I)V", 7);
        arm_shared(&Arc::from("(J)V"));
        assert_eq!(take().as_deref(), Some("(J)V"));
        assert_eq!(taken_caller(), 0, "a later arm resets the caller");
    }

    /// Round 15 wave 4 (lane mh2; MH15-2): a re-armed descriptor is the SAME
    /// shared text while it is among the last `RECENT_SITES` distinct ones, a
    /// fresh one after it falls out, and whatever the ring holds, what is
    /// taken is exactly what was last armed.
    #[test]
    fn a_rearmed_descriptor_is_shared_and_the_take_contract_holds() {
        let sites = ["(I)I", "(J)J", "(F)F", "(D)D", "(Z)Z"];
        for s in sites {
            arm("(I)I");
            arm(s);
            assert_eq!(take().as_deref(), Some(s), "the last armed wins");
            assert_eq!(take(), None);
        }
        let reuse = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_CALL_SITE_REUSE");
        arm("(Ljava/lang/Object;)V");
        let first = take().expect("armed");
        arm("(Ljava/lang/Object;)V");
        let again = take().expect("armed");
        assert_eq!(&*first, &*again);
        assert_eq!(Arc::ptr_eq(&first, &again), reuse, "shared while recent");
        // `RECENT_SITES` descriptors the ring does not hold push it out.
        let fresh = ["(B)B", "(S)S", "(C)C", "(Ljava/lang/String;)V"];
        assert_eq!(fresh.len(), RECENT_SITES);
        for s in fresh {
            arm(s);
        }
        clear();
        arm("(Ljava/lang/Object;)V");
        let evicted = take().expect("armed");
        assert_eq!(&*evicted, "(Ljava/lang/Object;)V");
        assert!(!Arc::ptr_eq(&first, &evicted), "a fresh copy once evicted");
    }

    /// `arm_shared` publishes the same text as `arm`, under the same TAKE
    /// contract, and hands the consumer the door's own allocation (a count,
    /// not a copy).
    #[test]
    fn a_shared_descriptor_is_taken_once_without_a_copy() {
        let site: Arc<str> = Arc::from("(LH;)I");
        arm_shared(&site);
        assert_eq!(Arc::strong_count(&site), 2, "the cell holds one count");
        let taken = take().expect("armed");
        assert!(
            Arc::ptr_eq(&taken, &site),
            "the door's allocation, not a copy"
        );
        assert_eq!(&*taken, "(LH;)I");
        assert_eq!(take(), None, "taken, not peeked");
        drop(taken);
        arm_shared(&site);
        clear();
        assert_eq!(Arc::strong_count(&site), 1, "clear drops the cell's count");
        assert_eq!(take(), None);
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

    /// Round 15 wave 3 (lane vh3; VH1-3): the widened word covers the
    /// coordinates, keeps the old cell as its low half, and shares its cell.
    #[test]
    fn the_param_site_word_covers_the_coordinates() {
        // `f.get(arr, i)` with `(VarHandle, int[], char)` declared: the call
        // site is `([II)I`, coordinates only.
        let w = vh_param_site_classes("([II)I");
        assert_eq!(vh_param_site_class_at(w, 0), b'I');
        assert_eq!(vh_param_site_class_at(w, 1), vh_site::OTHER);
        assert_eq!(vh_param_site_class_at(w, 2), vh_site::UNKNOWN);
        assert_eq!(
            vh_param_site_class_at(w, VH_PARAM_SITE_SLOTS),
            vh_site::UNKNOWN
        );
        // The low half is the old cell for every shape the old test pins.
        for d in [
            "(LR11$H;Ljava/lang/Object;)V",
            "(LR11$H;I)V",
            "(LR11$H;Ljava/lang/Long;Ljava/lang/Number;)Z",
            "(J)V",
            "([IILjava/lang/Integer;)V",
            "()V",
            "(Ljava/lang/Object",
        ] {
            assert_eq!(
                vh_param_site_classes(d) & 0xffff,
                u64::from(vh_value_site_classes(d)),
                "{d}"
            );
        }
        // Nine parameters: the first two fall off the word (seven slots; the
        // top byte is the return's, wave 57).
        let nine = "(BSCIJFDZLjava/lang/Object;)V";
        let w = vh_param_site_classes(nine);
        assert_eq!(vh_param_site_class_at(w, 0), vh_site::OBJECT);
        assert_eq!(vh_param_site_class_at(w, 6), b'C');
        assert_eq!(vh_param_site_class_at(w, 7), vh_site::UNKNOWN);
        assert_eq!(
            w >> VH_SITE_RETURN_SHIFT,
            0,
            "the classes never reach the return byte"
        );
        // One cell: a word armed is taken whole, a u16 armed reads zero above.
        arm_vh_param_site(w);
        assert_eq!(take_vh_param_site(), w);
        assert_eq!(take_vh_param_site(), 0);
        arm_vh_value_site(0x0f01);
        assert_eq!(take_vh_param_site(), 0x0f01);
        arm_vh_param_site(w);
        assert_eq!(u64::from(take_vh_value_site()), w & 0xffff);
        assert_eq!(
            take_vh_param_site(),
            0,
            "the u16 take clears the whole word"
        );
    }

    /// Wave 57, lane L4: the word carries the return in its top byte -- a
    /// primitive's descriptor byte, a reference's class code, nothing for
    /// `Object` or `void` -- and keeps the classes.
    #[test]
    fn the_param_site_word_carries_the_return() {
        for (d, r) in [
            ("(LH;)I", b'I'),
            ("(LH;I)J", b'J'),
            ("()S", b'S'),
            ("([II)Z", b'Z'),
            ("(LH;)Ljava/lang/Object;", 0),
            ("(LH;I)V", 0),
            ("(LH;", 0),
            ("(LH;)[I", vh_site::OTHER),
            ("(LH;)Ljava/lang/String;", vh_site::OTHER),
            ("(LH;)Ljava/lang/Long;", vh_site::W_LONG),
            ("(LH;)Ljava/lang/Number;", vh_site::NUMBER),
        ] {
            let w = vh_param_site_word(d);
            assert_eq!(vh_param_site_return(w), r, "{d}");
            assert_eq!(
                w & VH_PARAM_SITE_PARAM_MASK,
                vh_param_site_classes(d),
                "{d}"
            );
        }
        assert_eq!(vh_param_site_return(vh_param_site_classes("(LH;)I")), 0);
        // Every class code sits below every descriptor letter.
        assert!(vh_site::OTHER < b'B');
    }
}
