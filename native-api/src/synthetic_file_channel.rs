// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The private slot map of the **synthetic** `java.nio.channels.FileChannel`,
//! in one place, because two crates own accessors for it.
//!
//! # The defect this closes
//!
//! `native-io` allocated a literal `java/nio/channels/FileChannel` and kept
//! `{0: fd (Int), 1: position (Long)}` in its object fields; `native-builtins`
//! reads and writes the same two slots by literal index, in two registrars. The
//! real class declares four fields, all inherited from
//! `java.nio.channels.spi.AbstractInterruptibleChannel` (`javap -p`, JDK
//! 25.0.3.9, statics excluded, superclass fields first):
//!
//! ```text
//!   0 closeLock  Ljava/lang/Object;
//!   1 closed     Z            <- volatile; `isOpen()` is `return !closed`
//!   2 interruptor Lsun/nio/ch/Interruptible;
//!   3 interruptedTarget Ljava/lang/Object;
//! ```
//!
//! So the fd landed in the `final Object` real `close()` synchronizes on — a
//! reference-typed slot, where `gc::coerce_field_value_by_descriptor` degrades
//! an `Int` to null, so the fd never persisted at all — and the file position
//! landed in `closed`, a `boolean` that real JDK bytecode reads. A channel whose
//! position moved off zero therefore reported itself CLOSED to
//! `AbstractInterruptibleChannel.isOpen()`, `close()` and `begin()`.
//!
//! # Why it could not be fixed on one side
//!
//! W7-68-live-under-allocations.md refused a one-sided renumber, and was right
//! to: the map has two owning crates and three registrations, so moving one
//! side only moves the disagreement. This module is the other half of that
//! refusal — one owner, both crates forwarding to it, so the allocator and every
//! accessor cannot disagree about where the private slots start. It is the same
//! reasoning that put [`crate::appended_slots`] here rather than in
//! `native-builtins`.
//!
//! # The receiver screen is load-bearing
//!
//! Before this module the accessors were safe against a foreign receiver by
//! accident: `get_field(real_channel, 0)` answered the real `closeLock`
//! reference, and every call site's `as_int()` / `Value::Int(v)` match then
//! failed and took the "not open" arm. With the base applied, slot `base + 0` on
//! a real `sun.nio.ch.FileChannelImpl` is one of ITS declared fields, and a
//! write there would be the very corruption this module exists to prevent. So
//! the screen moved INSIDE the accessors: they answer `Value::Object(None)`
//! (and writes are dropped) unless the receiver's class is exactly
//! [`CLASS`]. Every call site inherits it, and none can forget it —
//! the "convert the idiom, not the sites" rule.
//!
//! A per-class base is sound here for the reason [`crate::appended_slots`]
//! requires it to be stated per call site: after the screen, every receiver
//! these accessors touch is an instance of exactly [`CLASS`], which only
//! `native_fc_open` and the `newFileChannel` legacy fallback produce.

use crate::appended_slots::{base_for_class, base_for_class_id};
use crate::registry::{NativeClassAccess, NativeContext, NativeHeapAccess};
use cratonvm_types::{ObjectRef, Value};

/// The one class these accessors serve. A receiver of any other class — a real
/// `sun.nio.ch.FileChannelImpl`, say — is not ours and is left alone.
pub const CLASS: &str = "java/nio/channels/FileChannel";

/// Private slot 0: the `fd_table` id, an `Int`. `-1`/absent means closed.
const PRIVATE_FD: usize = 0;
/// Private slot 1: the file position, a `Long`.
const PRIVATE_POS: usize = 1;
/// How many private slots the map needs.
const PRIVATE_WIDTH: usize = 2;

/// How many slots to ask the allocator for when minting a synthetic
/// `FileChannel`: every real declared field, then the private map above them.
///
/// In synthetic-JDK mode the class is a fabricated stub, `base_for_class`
/// answers 0, and this is `PRIVATE_WIDTH` — byte-identical to the old literal
/// `2`, which is why that arm needs no second code path.
#[must_use]
pub fn alloc_slots(ctx: &mut dyn NativeContext) -> usize {
    base_for_class(ctx, CLASS) + PRIVATE_WIDTH
}

/// The private base for `this`, or `None` when `this` is not one of ours.
///
/// Asks [`base_for_class_id`] with the receiver's own id rather than handing
/// `CLASS` back to [`base_for_class`]. The screen above has already proved the
/// two name the same class, and the id-taking form does not reach
/// `ensure_class_initialized` — so `fd_value`, `position_value` and their
/// setters are no longer `<clinit>` doors, which is what they were on every
/// call. Same base, same guard, no Java re-entry.
fn private_base(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let class_id = ctx.class_id_of_object(this);
    if ctx.class_name_of_id(class_id).as_deref() != Some(CLASS) {
        return None;
    }
    let base = base_for_class_id(ctx, class_id);
    // A stub or an unresolvable class gives base 0; either way the slot has to
    // exist on the object before we touch it. `alloc_object` clamps UP to the
    // declared width, so a receiver we allocated always satisfies this; a
    // receiver minted elsewhere at the bare declared width does not, and is
    // left alone rather than written past.
    (ctx.object_num_fields(this) > base + PRIVATE_POS).then_some(base)
}

/// The fd id as a `Value`, or `Value::Object(None)` when `this` is not ours.
///
/// Returning a `Value` rather than an `i32` is deliberate: every call site
/// already matches on `Value::Int(v)` or calls `.as_int()`, and the "not ours"
/// answer is exactly what those sites used to get from a reference-typed real
/// field. The conversion is therefore behaviour-preserving at every site that
/// receives a foreign receiver.
#[must_use]
pub fn fd_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    match private_base(ctx, this) {
        Some(base) => ctx.get_field(this, base + PRIVATE_FD),
        None => Value::Object(None),
    }
}

/// Store the fd id. A no-op on a receiver that is not ours.
pub fn set_fd_value(ctx: &mut dyn NativeContext, this: ObjectRef, value: Value) {
    if let Some(base) = private_base(ctx, this) {
        ctx.set_field(this, base + PRIVATE_FD, value);
    }
}

/// The file position as a `Value`, or `Value::Object(None)` when not ours.
#[must_use]
pub fn position_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    match private_base(ctx, this) {
        Some(base) => ctx.get_field(this, base + PRIVATE_POS),
        None => Value::Object(None),
    }
}

/// Store the file position. A no-op on a receiver that is not ours.
///
/// This is the write that used to land in `AbstractInterruptibleChannel.closed`
/// and make an open channel report itself closed.
pub fn set_position_value(ctx: &mut dyn NativeContext, this: ObjectRef, value: Value) {
    if let Some(base) = private_base(ctx, this) {
        ctx.set_field(this, base + PRIVATE_POS, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The width asked of the allocator must always leave room for both private
    /// slots, whatever the base. Guards the shape that cost a build in
    /// `socket_channel.rs` (`F_REUSEADDR = 11` with `N_FIELDS = 11`): an index
    /// outside the requested width is dropped on write and reads back as the
    /// zero value, which is indistinguishable from "never set".
    #[test]
    fn the_requested_width_covers_every_private_slot() {
        let mut ctx = crate::test_mock::MockNativeContext::new();
        let slots = alloc_slots(&mut ctx);
        assert!(slots > PRIVATE_FD, "fd slot outside requested width");
        assert!(slots > PRIVATE_POS, "position slot outside requested width");
        assert_eq!(
            slots,
            base_for_class(&mut ctx, CLASS) + PRIVATE_WIDTH,
            "allocator width and accessor base must be derived from one function"
        );
    }
}
