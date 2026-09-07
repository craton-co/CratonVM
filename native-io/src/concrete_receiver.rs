// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Minting a receiver of the class the real JDK would construct, instead of the
//! ABSTRACT public API class this crate's factories used to name.
//!
//! # The defect this closes
//!
//! `H21-1` measured it on one class and `WORKER-4` on twelve more: a factory
//! here would call `ensure_class_initialized("java/nio/channels/DatagramChannel")`
//! — a name that resolves, in a real-JDK image, to the **real, abstract** JDK
//! class — and then `alloc_object` on the result. The object handed back to the
//! application therefore has an ABSTRACT (or INTERFACE) runtime class. No `new`
//! opcode in any image can legally produce such a receiver: JVMS §6.5 makes it
//! an `InstantiationError`. It is a defect **with no oracle run required**
//! (`H21-1` N3), and `probes/W4Abstract.java` is the universal assertion that
//! finds them.
//!
//! # The two halves, and BOTH are required
//!
//! 1. **Mint the concrete class** — [`alloc_concrete`]. The concrete class is
//!    platform-dependent (`sun.nio.ch.UnixAsynchronousSocketChannelImpl` on
//!    Linux, `…Windows…` on Windows), so the candidate list is ordered and the
//!    first name the image actually has wins. The ABSTRACT name stays as the
//!    final fallback, because in synthetic-JDK mode the `sun.nio.ch` class is
//!    absent and the fabricated stub under the abstract name is what that mode
//!    has always used. `[refuse=transient?]` in reverse: do not delete a path
//!    whose absence you have not measured.
//!
//! 2. **Register the natives on the concrete class too.** Native dispatch keys
//!    on the RECEIVER's runtime class (`H11-1`, measured twice), and the one
//!    fallback walk runs only when the receiver's own class declares neither
//!    the method nor a registration. A real `sun.nio.ch.*Impl` DOES declare
//!    these methods, with `Code` — so moving the receiver without moving the
//!    registration does not fall back to the abstract row, it runs the JDK's
//!    own bytecode against state this VM never initialised. Every caller of
//!    [`alloc_concrete`] must add the same names to its registrar.
//!
//! # The slot map, and why it is not a renumbering
//!
//! An abstract public class declares few or no instance fields, so a native
//! that keeps private state at slots `0..N` was — accidentally — not colliding
//! with anything. The concrete `Impl` declares many. The remedy is the one
//! `native-api`'s `appended_slots` module already implements for exactly this
//! species: start the private map ABOVE every declared field and allocate
//! `base + N` slots. [`concrete_base`] is the accessor-side twin, with the
//! width guard that collapses the base to 0 for any receiver this crate did not
//! allocate (a stub-mode object, or a real `Impl` built by JDK bytecode) — so
//! such a receiver reads exactly the slots it read before this change, never a
//! new refusal and never an out-of-range access.
//!
//! Families whose state lives in an identity-keyed SIDE TABLE (`DatagramChannel`,
//! `SocketChannel`, `Selector`) need none of that and take the class swap
//! unchanged; the module doc of each says which it is.

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ClassId, ObjectRef};

/// A receiver minted as the concrete class, and where its private slot map
/// starts on that class.
#[derive(Clone, Copy, Debug)]
pub struct Concrete {
    /// The freshly allocated object.
    pub obj: ObjectRef,
    /// First private slot index. `0` when the resolved class is a fabricated
    /// stub (its fields ARE `_f0.._fN`) or when nothing resolved.
    pub base: usize,
}

/// The JVMS §6.5 predicate and the candidate walk both live in
/// `cratonvm_native_api::instantiable` — the crate that also owns
/// `appended_slots`, and the only one both this crate and `native-builtins`'
/// `getClass()` alias table can reach. See that module for why the check is on
/// the CLASS and never on the candidate NAME.
use cratonvm_native_api::instantiable::first_instantiable;

/// Mint `nfields` private slots on the first of `candidates` present in the
/// image, falling back to `abstract_name`.
///
/// `candidates` is ordered most-specific-first and is normally the
/// platform-specific `sun.nio.*` implementation followed by its portable
/// siblings; `abstract_name` is the public API class this factory used to name
/// and is what synthetic-JDK mode still gets.
///
/// The returned [`Concrete::base`] is resolved BEFORE the allocation, so no GC
/// can run between deciding the base and writing through it.
pub fn alloc_concrete(
    ctx: &mut dyn NativeContext,
    candidates: &[&str],
    abstract_name: &str,
    nfields: usize,
) -> Concrete {
    if let Some((name, cid)) = first_instantiable(ctx, candidates) {
        let base = cratonvm_native_api::appended_slots::base_for_class(ctx, name);
        // `alloc_object` clamps UP to the class's declared width, so asking for
        // `base + nfields` is what makes the private slots both in bounds and
        // non-aliasing.
        let obj = ctx.alloc_object(cid, base + nfields);
        return Concrete { obj, base };
    }
    // The abstract/stub path, byte-identical to what this crate did before.
    let base = cratonvm_native_api::appended_slots::base_for_class(ctx, abstract_name);
    let cid = ctx
        .ensure_class_initialized(abstract_name)
        .unwrap_or_else(|_| ClassId::new(0));
    let obj = ctx.alloc_object(cid, base + nfields);
    Concrete { obj, base }
}

/// Where `this`'s private slot map starts — the accessor-side twin of
/// [`alloc_concrete`].
///
/// The width guard is load-bearing in BOTH directions, and this is the same
/// guard `pipe.rs::channel_private_base` documents:
///
///   * a receiver this crate did NOT allocate (a real `Impl` built by JDK
///     bytecode, or a stub-mode object) is too narrow for `base + nfields`, so
///     the base collapses to 0 and the accessor reads the slots it read before
///     this change;
///   * the class-resolution-FAILED arm of [`alloc_concrete`] allocates against
///     `ClassId::new(0)`, which the VM substitutes with a
///     `cratonvm/synthetic/AnonymousObject$N` declaring exactly `nfields`
///     fields. A later `base_for_class` on that receiver would answer `nfields`
///     and disagree with the 0 the allocator used; the width check sends it
///     back to 0, which is the base that was actually used.
///
/// `&dyn`, not `&mut dyn`, and that is load-bearing rather than tidiness: it is
/// what makes "no GC runs while resolving a private-slot base" a COMPILE ERROR
/// to violate. Until 2026-09-07 this path reached `ensure_class_initialized`
/// and therefore `<clinit>`, so an ordinary private field read was a Java
/// re-entry that could move — or under the generational young sweep zero — every
/// unpinned `ObjectRef` its caller was holding. Widening this back to `&mut`
/// would silently make that possible again; the borrow checker is the only
/// guard that survives a reader who has not read this comment.
pub fn concrete_base(ctx: &dyn NativeContext, this: ObjectRef, nfields: usize) -> usize {
    cratonvm_native_api::appended_slots::base_for_object(ctx, this, nfields)
}

/// Copy every registration this registrar added for `from` since `since` onto
/// `to`, preserving each row's `NativeKind`.
///
/// # Why a mirror rather than a hand-written second table
///
/// The two spellings must carry an IDENTICAL triple set or the family answers
/// for one receiver and not the other, and a hand-maintained copy of forty
/// rows drifts on the first row somebody adds to only one of them —
/// `socket_channel.rs`' `for c in [sc, scimpl]` is the same idea expressed
/// where the registrations happen to be uniform enough for a loop. Here they
/// are not: the `DatagramSocketAdaptor` and `SelectorProvider` rows are
/// interleaved with the channel's own, and must NOT be mirrored.
///
/// # `since` is load-bearing, and is why this takes an index
///
/// The registry is shared by every crate. Filtering on the class name alone
/// would also copy rows some OTHER crate registered on `from` — attributing
/// them to this one and, worse, handing `to`'s slot to a body with an
/// unrelated field layout. `[2 producers, 1 slot]` / `[dup nati]`. `since` is
/// `dump_registrations().len()` taken at the top of the registrar, so the
/// mirror can only ever see rows this function itself wrote.
pub fn mirror_class_registrations(
    r: &mut cratonvm_native_api::NativeMethodRegistry,
    since: usize,
    from: &str,
    to: &str,
) {
    // Collected as owned data first: `dump_registrations` borrows the registry
    // immutably and `register_with_kind` needs it mutably.
    let rows: Vec<(String, String, cratonvm_native_api::NativeKind)> = r
        .dump_registrations()
        .into_iter()
        .skip(since)
        .filter(|(c, _, _, _)| *c == from)
        .map(|(_, m, d, k)| (m.to_string(), d.to_string(), k))
        .collect();
    for (method, descriptor, kind) in rows {
        let Some(id) = r.resolve_id(from, &method, &descriptor) else {
            continue;
        };
        let Some(callback) = r.callback_of(id) else {
            continue;
        };
        r.register_with_kind(to, &method, &descriptor, callback, kind);
    }
}
