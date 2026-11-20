use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn make_bb(ctx: &mut MockNativeContext, cap: usize) -> ObjectRef {
    // alloc_byte_buffer leaves pos=0, lim=cap, cap=cap.
    alloc_byte_buffer(ctx, cap)
}

/// W7-58 — the direct arm, at the exact receiver shape that used to fail.
///
/// A real `java.nio.DirectByteBuffer` has no `hb`, and slot 0 is
/// `Buffer.mark` = -1. `bb_state` fell through to it and raised
/// "ByteBuffer missing backing array (field 0 returned Int(-1))". This
/// pins the mark AS -1 so the test cannot pass by the slot happening to
/// hold something else.
#[test]
fn bb_state_resolves_a_direct_buffer_through_address() {
    let mut ctx = MockNativeContext::new();
    let mut native = vec![5u8, 6, 7, 8];
    let bb = ctx.alloc_object(8);
    ctx.set_field(bb, 0, Value::Int(-1)); // mark — the slot that used to be read as `hb`
    ctx.set_field(bb, 1, Value::Int(1)); // position
    ctx.set_field(bb, 2, Value::Int(3)); // limit
    ctx.set_field(bb, 3, Value::Int(4)); // capacity
    ctx.set_field(bb, 4, Value::Long(native.as_mut_ptr() as i64)); // address

    let view = bb_state(&ctx, bb).expect("direct buffer must resolve, not error");
    assert!(matches!(view.storage, BbStorage::Direct { .. }));
    assert_eq!((view.pos, view.lim, view.cap), (1, 3, 4));
    assert_eq!(tb_read_elem(&ctx, view, 0).unwrap(), Value::Int(5));
    assert_eq!(tb_read_elem(&ctx, view, 3).unwrap(), Value::Int(8));
    tb_write_elem(&mut ctx, view, 2, Value::Int(-1)).unwrap();
    assert_eq!(native[2], 0xFF);
}

/// `remaining()` / `hasRemaining()` are `limit - position` and
/// `position < limit`. They are defined on `java.nio.Buffer`, above any
/// notion of storage, and must answer on a buffer with NO resolvable
/// storage at all — the receiver here has neither array nor address, which
/// is strictly harder than the direct case that broke `RJdkDefineClass`.
#[test]
fn remaining_needs_no_backing_storage() {
    let mut ctx = MockNativeContext::new();
    let bb = ctx.alloc_object(8);
    ctx.set_field(bb, 0, Value::Int(-1)); // mark
    ctx.set_field(bb, 1, Value::Int(2)); // position
    ctx.set_field(bb, 2, Value::Int(9)); // limit
    ctx.set_field(bb, 3, Value::Int(16)); // capacity

    assert!(
        bb_state(&ctx, bb).is_err(),
        "this receiver genuinely has no storage — if bb_state stopped \
         refusing it, the assertions below would prove nothing"
    );
    assert_eq!(
        native_bb_remaining(&mut ctx, &[Value::Object(Some(bb))]).unwrap(),
        Some(Value::Int(7))
    );
    assert_eq!(
        native_bb_has_remaining(&mut ctx, &[Value::Object(Some(bb))]).unwrap(),
        Some(Value::Int(1))
    );
}

/// A HEAP buffer's `Buffer.address` is `ARRAY_BYTE_BASE_OFFSET + offset`
/// (16 for a fresh one), not a process pointer. If the array fails to
/// resolve, the direct arm must NOT accept 16 and dereference it —
/// `addr=0x10` was 51 of the 53 crashes in the 2026-08-10 H2 sweep.
#[test]
fn direct_arm_refuses_an_array_base_offset_as_a_pointer() {
    let mut ctx = MockNativeContext::new();
    let bb = ctx.alloc_object(8);
    ctx.set_field(bb, 0, Value::Int(-1));
    ctx.set_field(bb, 1, Value::Int(0));
    ctx.set_field(bb, 2, Value::Int(4));
    ctx.set_field(bb, 3, Value::Int(4));
    ctx.set_field_by_name(bb, "address", Value::Long(16));
    assert!(bb_state(&ctx, bb).is_err());
    assert!(bb_storage_view(&ctx, bb).is_err());
    assert!(!is_plausible_native_addr(16));
    assert!(is_plausible_native_addr(0x1_0000));
}

/// A DIRECT typed view is byte-addressed: its element index has to be
/// scaled by the element width, and the bytes decoded in the receiver's
/// byte order. `ByteBufferAsIntBufferL` / `...B` is where real JDK 25 puts
/// that order, there being no field to hold it.
#[test]
fn direct_typed_view_scales_by_element_width_and_honours_order() {
    let mut ctx = MockNativeContext::new();
    // 0x01020304 big-endian, then 0x05060708 big-endian.
    let mut native = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    let addr = native.as_mut_ptr() as i64;

    let be = ctx.alloc_object_with_class(8, "java/nio/ByteBufferAsIntBufferB");
    ctx.set_field(be, 1, Value::Int(0));
    ctx.set_field(be, 2, Value::Int(2));
    ctx.set_field(be, 3, Value::Int(2));
    ctx.set_field_by_name(be, "address", Value::Long(addr));
    let bev = bb_state(&ctx, be).unwrap();
    assert_eq!(tb_elem_width(bev.elem), 4);
    assert!(bev.big_endian);
    assert_eq!(tb_read_elem(&ctx, bev, 0).unwrap(), Value::Int(0x01020304));
    // Element 1, not byte 1 — the whole point of the width scaling.
    assert_eq!(tb_read_elem(&ctx, bev, 1).unwrap(), Value::Int(0x05060708));

    let le = ctx.alloc_object_with_class(8, "java/nio/ByteBufferAsIntBufferL");
    ctx.set_field(le, 1, Value::Int(0));
    ctx.set_field(le, 2, Value::Int(2));
    ctx.set_field(le, 3, Value::Int(2));
    ctx.set_field_by_name(le, "address", Value::Long(addr));
    let lev = bb_state(&ctx, le).unwrap();
    assert!(!lev.big_endian);
    assert_eq!(tb_read_elem(&ctx, lev, 0).unwrap(), Value::Int(0x04030201));

    // Round-trip a write through the little-endian view.
    tb_write_elem(&mut ctx, lev, 1, Value::Int(0x0A0B0C0D)).unwrap();
    assert_eq!(&native[4..8], &[0x0D, 0x0C, 0x0B, 0x0A]);
}

/// `Char` is unsigned and `Short` is signed. They share a width, so the
/// only thing that separates them is the sign extension, and getting it
/// wrong is invisible until a value with the high bit set goes through.
#[test]
fn direct_char_is_unsigned_and_short_is_signed() {
    let mut ctx = MockNativeContext::new();
    let mut native = vec![0xFFu8, 0xFE];
    let addr = native.as_mut_ptr() as i64;

    let cb = ctx.alloc_object_with_class(8, "java/nio/DirectCharBufferL");
    ctx.set_field(cb, 3, Value::Int(1));
    ctx.set_field_by_name(cb, "address", Value::Long(addr));
    let cv = bb_state(&ctx, cb).unwrap();
    assert_eq!(tb_read_elem(&ctx, cv, 0).unwrap(), Value::Int(0xFEFF));

    let sb = ctx.alloc_object_with_class(8, "java/nio/DirectShortBufferL");
    ctx.set_field(sb, 3, Value::Int(1));
    ctx.set_field_by_name(sb, "address", Value::Long(addr));
    let sv = bb_state(&ctx, sb).unwrap();
    assert_eq!(tb_read_elem(&ctx, sv, 0).unwrap(), Value::Int(-257));
}

/// `array()` on a typed buffer used to return raw slot 0, i.e.
/// `Buffer.mark` on any real-layout receiver — an `Int(-1)` returned where
/// the descriptor promises `[I`. A direct receiver must throw instead.
#[test]
fn typed_array_accessor_refuses_a_direct_receiver_and_never_returns_mark() {
    let mut ctx = MockNativeContext::new();
    let mut native = vec![0u8; 8];
    let bb = ctx.alloc_object_with_class(8, "java/nio/DirectIntBufferU");
    ctx.set_field(bb, 0, Value::Int(-1)); // mark
    ctx.set_field(bb, 3, Value::Int(2));
    ctx.set_field_by_name(bb, "address", Value::Long(native.as_mut_ptr() as i64));
    let r = native_tb_array(&mut ctx, &[Value::Object(Some(bb))]);
    assert!(r.is_err(), "direct array() must throw, got {r:?}");

    // And the heap case still answers with the array itself.
    let heap = alloc_typed_buffer(&mut ctx, "java/nio/IntBuffer", ArrayElementType::Int, 3);
    match native_tb_array(&mut ctx, &[Value::Object(Some(heap))]).unwrap() {
        Some(Value::Object(Some(arr))) => assert_eq!(ctx.array_length(arr), 3),
        other => panic!("heap array() must return the backing array, got {other:?}"),
    }
}

// ---- the hasArray/array/arrayOffset three-way split (F14) ---------------
//
// These pin `buffer_array_access`, NOT a receiver. Deliberate:
// `MockNativeContext` resolves field NAMES through a slot fallback, so a
// receiver-shaped assertion about `isReadOnly` here would be measuring the
// mock's name-to-slot table rather than the ordering rule. The four
// natives' own storage classification is covered by the receiver tests
// above; what could silently regress is the ORDER, and the order is here.

/// The truth table, transcribed from the JDK body. MEASURED on
/// jdk-25.0.3+9 (`probes/BufferAccessibleArrayProbe`): `HeapByteBuffer`
/// hands the array over, `HeapByteBufferR` throws `ReadOnlyBuffer`,
/// `DirectByteBuffer` and `ByteBufferAsIntBufferB` throw
/// `UnsupportedOperation`.
#[test]
fn buffer_array_access_is_the_jdk_three_way_table() {
    assert_eq!(
        buffer_array_access(true, false),
        BufferArrayAccess::Accessible,
        "array-backed and writable is the ONLY state that hands the array over"
    );
    assert_eq!(
        buffer_array_access(true, true),
        BufferArrayAccess::ReadOnly,
        "HeapByteBufferR: hb non-null, isReadOnly set -> ReadOnlyBufferException"
    );
    assert_eq!(
        buffer_array_access(false, false),
        BufferArrayAccess::Absent,
        "ByteBufferAsIntBufferB is WRITABLE and array-less -> UnsupportedOperation, \
         which is why this is not a read-only-versus-not test"
    );
}

/// The one cell a read-only-first implementation gets wrong, and the reason
/// this is a function instead of two booleans at each call site.
/// `DirectByteBufferR` (and `CharBuffer.wrap(String))` is read-only AND
/// array-less; MEASURED answer is `UnsupportedOperationException`.
#[test]
fn buffer_array_access_checks_absent_before_read_only() {
    assert_eq!(
        buffer_array_access(false, true),
        BufferArrayAccess::Absent,
        "hb == null is checked FIRST; a read-only-first implementation answers \
         ReadOnly here and is right about every other cell"
    );
}

/// Exactly one of the four states may hand the array out. The wrong
/// CAPABILITY this fix removes was a second one doing so.
#[test]
fn buffer_array_access_hands_the_array_out_for_exactly_one_state() {
    let all = [
        buffer_array_access(false, false),
        buffer_array_access(false, true),
        buffer_array_access(true, false),
        buffer_array_access(true, true),
    ];
    assert_eq!(
        all.iter()
            .filter(|s| **s == BufferArrayAccess::Accessible)
            .count(),
        1
    );
}

/// `buffer_no_backing_array` must use the NO-ARGUMENT constructor. The empty
/// string is the spelling `types/src/error.rs` maps to `None`, i.e. a null
/// `getMessage()`; MEASURED, HotSpot's is null on all four UOE arms.
/// A non-empty message here would be invisible to every `catch` and to any
/// class-name assertion — only a message-exact differential sees it.
#[test]
fn bb_no_backing_array_carries_no_detail_message() {
    match buffer_no_backing_array() {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::UnsupportedOperationException { message },
        )) => assert!(
            message.is_empty(),
            "the EMPTY message is the ()V-constructor spelling; \
             \"direct buffer has no backing array\" would give getMessage() a \
             non-null value HotSpot does not have, and no catch would ever see it. \
             Got {message:?}"
        ),
        other => panic!("must be UnsupportedOperationException, got {other:?}"),
    }
}

// ---- read-only contagion across derived views (F21) ---------------------
//
// Same discipline as the block above: these pin `buffer_view_read_only`,
// the pure decision, and NOT a receiver. `MockNativeContext` resolves field
// NAMES through a slot fallback, so a receiver-shaped assertion about
// `isReadOnly` measures the mock's name-to-slot table, not the rule.
//
// Every row below is a pasted cell from
// `scratchpad/f21/F21ViewContagionProbe.java` on jdk-25.0.3+9, and every
// negative row is what a SPECIFIC wrong implementation answers — never a
// negated condition.

/// `duplicate()` / `slice()` / `slice(int,int)` INHERIT the source's flag,
/// in both directions.
///
/// MEASURED, all seven families, byte-identical transcripts:
/// `<fam>.ro.dup.isReadOnly OK true`, `<fam>.ro.slice.isReadOnly OK true`,
/// `<fam>.ro.slice(1,2).isReadOnly OK true`, and the writable source's
/// three counterparts `OK false`.
#[test]
fn derived_views_inherit_read_only_in_both_directions() {
    assert!(
        buffer_view_read_only(BufferDerivation::Inherit, true),
        "readOnlyBuffer.duplicate() is a HeapByteBufferR. The PRE-FIX \
         `native_bb_duplicate` wrote nothing to isReadOnly, so it answered \
         false here and re-opened the mutable alias F14-1 closed"
    );
    assert!(
        !buffer_view_read_only(BufferDerivation::Inherit, false),
        "and it is not contagious UPWARD: hcb.duplicate().isReadOnly() is \
         false. An implementation that stamped every derived view read-only \
         would pass the row above and fail this one"
    );
}

/// `asReadOnlyBuffer()` is UNCONDITIONAL, which is what makes it one-way.
///
/// MEASURED: `<fam>.w.aro.isReadOnly OK true`, `<fam>.ro.aro.isReadOnly OK
/// true`, and `<fam>.ro.dup.aro.isReadOnly OK true` — no composition of JDK
/// buffer operations returns to writable, and there is no
/// `asWritableBuffer` to try.
#[test]
fn as_read_only_buffer_is_unconditional_and_therefore_one_way() {
    assert!(buffer_view_read_only(
        BufferDerivation::ForceReadOnly,
        false
    ));
    assert!(buffer_view_read_only(BufferDerivation::ForceReadOnly, true));
    // The mutant: `$ro_fn` implemented as a plain `$dup_fn` alias, which is
    // exactly what `servlet.rs`'s typed-view `$ro` still is. It agrees with
    // the read-only-source row and disagrees with the writable-source one,
    // so a test that only exercised a read-only source would let it live.
    assert_ne!(
        buffer_view_read_only(BufferDerivation::ForceReadOnly, false),
        buffer_view_read_only(BufferDerivation::Inherit, false),
        "asReadOnlyBuffer() is not duplicate(); the two differ on exactly \
         one of the four cells and that cell is the whole method"
    );
}

/// Three of the four cells are read-only, and the one writable cell is the
/// one an `Inherit` op applied to a writable source produces.
///
/// The counting shape is deliberate. An implementation that dropped the
/// flag on `Inherit` (the pre-fix state) gives two writable cells; one that
/// forced it everywhere gives zero. Both are caught here without the test
/// naming either.
#[test]
fn exactly_one_of_the_four_derivation_cells_is_writable() {
    let all = [
        buffer_view_read_only(BufferDerivation::Inherit, false),
        buffer_view_read_only(BufferDerivation::Inherit, true),
        buffer_view_read_only(BufferDerivation::ForceReadOnly, false),
        buffer_view_read_only(BufferDerivation::ForceReadOnly, true),
    ];
    assert_eq!(all.iter().filter(|ro| !**ro).count(), 1);
}

/// The CONTROL, and the reason the rows above compare booleans rather than
/// exception hierarchies.
///
/// `java.nio.ReadOnlyBufferException` **extends**
/// `java.lang.UnsupportedOperationException` (MEASURED:
/// `ROBE.super OK java.lang.UnsupportedOperationException`, and
/// `jdk25src/java.base/java/nio/ReadOnlyBufferException.java:40`). So an
/// `instanceof`-shaped or variant-shaped assertion that a read-only
/// receiver "throws UnsupportedOperationException" PASSES when the
/// implementation wrongly throws `ReadOnlyBufferException`, and only fails
/// in the other direction. That is why `buffer_array_access` returns a
/// three-valued enum compared with `assert_eq!` instead of the natives
/// being tested through a `matches!` on the error variant — and why this
/// lane's fixture NOMINATIONS compare exact class NAMES.
///
/// F5-1 §1 states these two "share no supertype below `RuntimeException`".
/// That is false. Its conclusion survives on the ORDER of the two checks,
/// not on the hierarchy; a later lane that checks the hierarchy claim,
/// finds it false, and concludes the distinction does not matter would be
/// wrong about something F5-1 got right.
#[test]
fn the_two_refusals_are_distinct_states_even_though_one_subclasses_the_other() {
    assert_ne!(
        buffer_array_access(true, true),
        buffer_array_access(false, true),
        "ReadOnly and Absent must stay distinct HERE, because they are NOT \
         distinguishable by a catch or an instanceof once they become \
         exceptions: ReadOnlyBufferException IS an \
         UnsupportedOperationException"
    );
}

// =====================================================================
// F26-1 — the ALIASING contract of the derived-buffer methods.
//
// These pin the three pure window functions and `heap_buffer_address`,
// NOT a receiver, and deliberately so: `MockNativeContext`'s name-to-slot
// fallback would make an assertion about `arrayOffset()` a measurement of
// the mock's field table rather than of the geometry. Every expected value
// is a transcript line from `scratchpad/f26/F26AliasProbe.java` /
// `scratchpad/f26/W.java` on `openjdk 25.0.3 2026-04-21 LTS
// (25.0.3+9-LTS)`, and each assertion names the wrong implementation it
// kills.
// =====================================================================

/// `slice()`'s window, MEASURED: `allocate(8).position(2).slice()` gives
/// `pos/lim/cap = 0/6/6` and `arrayOffset() == 2`.
#[test]
fn slice_window_is_the_jdk_heap_byte_buffer_slice_constructor_call() {
    assert_eq!(
        slice_window(0, 2, 8),
        BufferWindow {
            offset: 2,
            pos: 0,
            lim: 6,
            cap: 6,
            mark: -1
        }
    );
}

/// **The mutant that a copying implementation IS.** A copy can only ever
/// present `offset == 0`; the JDK presents `pos + offset`. Two rows,
/// because a single-source test cannot tell "forgot `offset`" from
/// "forgot `pos`".
#[test]
fn slice_window_offset_composes_and_neither_term_may_be_dropped() {
    // MEASURED: `hbb.slice().slice(1,2).arrayOffset()` is 3 — the outer
    // slice's offset 2 plus the inner index 1. A body that dropped the
    // source's own offset would answer 1 here.
    assert_eq!(slice_range_window(2, 1, 2).offset, 3);
    // MEASURED: `wrap(raw,2,4).slice().arrayOffset()` is 2. `wrap` leaves
    // `offset` at 0 and puts the 2 in POSITION, so a body that dropped the
    // position term would answer 0 — which is also exactly what the
    // copying implementation answered.
    assert_eq!(slice_window(0, 2, 6).offset, 2);
    // Both terms present at once: neither mutant survives this row.
    assert_eq!(slice_window(4, 3, 9).offset, 7);
}

/// MEASURED: `allocate(4).position(4).slice().arrayOffset()` is `4` even
/// though the slice is EMPTY. So `offset` moves with `position`
/// unconditionally — a body that computed it only when something remained
/// would answer 0 and pass every non-empty row above.
#[test]
fn slice_window_moves_the_offset_even_when_nothing_remains() {
    let w = slice_window(0, 4, 4);
    assert_eq!(w.cap, 0, "the empty slice really is empty");
    assert_eq!(w.offset, 4, "and its offset still moved");
}

/// `slice()` DISCARDS the mark and re-bases position; `duplicate()` keeps
/// both. MEASURED: `dup` then `reset()` → position 3; `slice` then
/// `reset()` → `java.nio.InvalidMarkException`.
///
/// The mutant this kills is the natural refactor: implementing `slice` by
/// calling `duplicate_window` and overwriting `cap`. That leaves the mark
/// and the position behind.
#[test]
fn slice_drops_the_mark_and_position_where_duplicate_keeps_them() {
    let s = slice_window(0, 5, 8);
    assert_eq!((s.pos, s.mark), (0, -1));
    let d = duplicate_window(0, 5, 8, 8, 3);
    assert_eq!((d.pos, d.mark), (5, 3));
    assert_ne!(
        (s.pos, s.mark),
        (d.pos, d.mark),
        "slice() and duplicate() are not the same window; a shared \
         implementation that differed only in `cap` would tie these"
    );
}

/// `slice(int,int)` is ABSOLUTE — `position` plays no part.
///
/// MEASURED: `allocate(8).position(2).slice(3,4)` gives `0/4/4` and
/// `arrayOffset() == 3`. **Three, not five.** A body that reused
/// `slice_window`'s `pos + offset` would answer 5, and would agree with the
/// relative form on every receiver whose position happens to be 0 — which
/// is most of them.
#[test]
fn slice_range_window_ignores_position_and_that_is_the_whole_difference() {
    assert_eq!(
        slice_range_window(0, 3, 4),
        BufferWindow {
            offset: 3,
            pos: 0,
            lim: 4,
            cap: 4,
            mark: -1
        }
    );
    assert_ne!(
        slice_range_window(0, 3, 4).offset,
        slice_window(0, 2, 8).offset + 3,
        "slice(index,length) must not fold in the source position"
    );
}

/// **THE LIVE BUG this record closes.** `duplicate()` carries `offset`
/// across unchanged; the shipped body left it at the fresh allocation's
/// `0` while sharing the array, so every read came from the wrong end of
/// the array.
///
/// MEASURED: `wrap(raw,2,4).slice().duplicate().arrayOffset()` is `2`, and
/// its `get(0)` equals `raw[2]`.
#[test]
fn duplicate_window_carries_the_source_offset() {
    assert_eq!(duplicate_window(2, 0, 4, 4, -1).offset, 2);
    assert_ne!(
        duplicate_window(2, 0, 4, 4, -1).offset,
        0,
        "the mutant is the shipped body: share the array, drop the offset"
    );
}

/// `duplicate()` keeps the source's CAPACITY, not its limit — MEASURED
/// `allocate(8).position(2).duplicate()` is `2/8/8`, not `2/8/6`.
#[test]
fn duplicate_window_keeps_capacity_not_limit() {
    let d = duplicate_window(0, 2, 6, 8, -1);
    assert_eq!((d.pos, d.lim, d.cap), (2, 6, 8));
    assert_ne!(d.cap, d.lim, "a body that passed `lim` as `cap` truncates");
}

/// The three windows are pairwise distinct on the SAME source, which is
/// the property that stops one of them being implemented as another.
///
/// Source: `offset = 1`, `pos = 2`, `lim = 6`, `cap = 8`, `mark = 3`.
#[test]
fn the_three_derivation_windows_are_pairwise_distinct() {
    let s = slice_window(1, 2, 6);
    let s2 = slice_range_window(1, 2, 6);
    let d = duplicate_window(1, 2, 6, 8, 3);
    assert_ne!(s, s2);
    assert_ne!(s, d);
    assert_ne!(s2, d);
    // And the two slices, which DO share `pos`/`mark`, differ on the pair
    // that matters: `slice()` measures from `pos` to `lim` (4 elements),
    // `slice(2,6)` takes 6 from index 2.
    assert_eq!((s.offset, s.cap), (3, 4));
    assert_eq!((s2.offset, s2.cap), (3, 6));
}

/// A negative or nonsensical source position must not underflow the
/// `usize` offset. `slice_window(0, -4, 8)` on an `i32`-to-`usize` cast
/// would give an offset near `usize::MAX`, i.e. an unbounded array index.
#[test]
fn slice_window_clamps_a_negative_position_instead_of_wrapping() {
    let w = slice_window(0, -4, 8);
    assert_eq!(w.offset, 0);
    assert_eq!(w.cap, 8);
}

/// `limit < position` yields an EMPTY window, never a negative capacity —
/// the JDK's own `(pos <= lim ? lim - pos : 0)`.
#[test]
fn slice_window_of_an_inverted_range_is_empty_not_negative() {
    let w = slice_window(0, 6, 2);
    assert_eq!(w.cap, 0);
    assert_eq!(w.lim, 0);
    assert_eq!(w.offset, 6, "the offset still moves to the position");
}

/// `Buffer.address` for a heap buffer is `ARRAY_BYTE_BASE_OFFSET + offset`,
/// not a bare 16 — real `HeapByteBuffer` computes
/// `ARRAY_BASE_OFFSET + off * ARRAY_INDEX_SCALE` and `ARRAY_INDEX_SCALE` is
/// 1 for `byte` (`jdk25src/java.base/java/nio/HeapByteBuffer.java`).
#[test]
fn heap_buffer_address_adds_the_array_base_offset_to_the_window_offset() {
    assert_eq!(heap_buffer_address(0), 16);
    assert_eq!(heap_buffer_address(2), 18);
    assert_ne!(
        heap_buffer_address(2),
        heap_buffer_address(0),
        "the mutant is the pre-fix literal `16`, which is right for a fresh \
         allocation and wrong for every aliasing view"
    );
}

// =====================================================================
// F37-1 §2 — the TYPED families' aliasing geometry.
//
// These pin the two things that differ from the `ByteBuffer` half:
// the window arithmetic is UNIT-AGNOSTIC (so F26's three functions are
// reused unchanged, in ELEMENTS), and the `address` seed is the ONE
// place the element width enters. Executed under `rustc --test` as
// `scratchpad/f37/geom.rs`: 21 pass, 20 mutants all die (§7 of the
// record).
// =====================================================================

/// The same composition as `slice_window_offset_composes_and_neither_term
/// _may_be_dropped`, but through `slice()` TWICE and with a NON-ZERO
/// source offset in the first call.
///
/// This row exists because the mutation run found a gap: every other
/// `slice_window` fixture passes `src_offset == 0`, where `pos` and
/// `src_offset + pos` are the same number, so a body that dropped the
/// source's offset entirely passed all of them. A fixture set is only as
/// strong as its non-degenerate inputs.
///
/// MEASURED, jdk-25.0.3+9, all six typed families identical:
/// `<fam>.allocate(8).position(2).slice().slice(1,2).arrayOffset()` is
/// `3`, and `<fam>.w.slice().duplicate().arrayOffset()` is `2`.
#[test]
fn a_typed_slice_of_a_slice_adds_both_offsets() {
    let first = slice_window(0, 2, 8);
    let second = slice_window(first.offset, 1, 6);
    assert_eq!(second.offset, 3, "2 + 1 == 3 ELEMENTS, not 1");
    assert_eq!((second.pos, second.lim, second.cap), (0, 5, 5));
    let third = duplicate_window(second.offset, 0, 5, 5, -1);
    assert_eq!(third.offset, 3, "and duplicate() carries all of it");
}

/// Real `HeapIntBuffer`'s protected constructor
/// (`jdk25src/java.base/java/nio/HeapIntBuffer.java:111`):
/// `this.address = ARRAY_BASE_OFFSET + off * ARRAY_INDEX_SCALE;`
///
/// Every width is a separate row, because the failure this catches is a
/// single-width implementation that passes one family and silently
/// mis-strides the other five — which is exactly what reusing
/// [`heap_buffer_address`] here would be.
#[test]
fn typed_buffer_address_scales_the_offset_by_the_element_width() {
    assert_eq!(typed_buffer_address(0, 4), 16, "a fresh buffer is the base");
    assert_eq!(typed_buffer_address(3, 1), 19);
    assert_eq!(typed_buffer_address(3, 2), 22);
    assert_eq!(typed_buffer_address(3, 4), 28);
    assert_eq!(typed_buffer_address(3, 8), 40);
}

/// The byte-width case must keep answering exactly what
/// [`heap_buffer_address`] answers, or the two seeds have drifted — the
/// failure this codebase keeps re-finding in families that grew a second
/// copy of one rule.
#[test]
fn the_byte_element_width_agrees_with_the_byte_buffer_address_seed() {
    for offset in [0usize, 1, 2, 7, 4096] {
        assert_eq!(
            typed_buffer_address(offset, tb_elem_width(ArrayElementType::Byte)),
            heap_buffer_address(offset),
            "width-1 must be the ByteBuffer seed at offset {offset}"
        );
    }
}

/// A DIRECT typed view has no `offset` field and no backing array: its
/// advanced `address` IS its window, and the advance is in BYTES from an
/// ELEMENT count.
///
/// The `rel_start == 0` row is `duplicate()` and the rest are `slice()`.
/// The mutant that answers `addr` unconditionally — which is what
/// `native-builtins`' `s2_*_dup` does, correctly, for a duplicate — is
/// wrong for every slice, and that is the one this row kills.
#[test]
fn typed_direct_view_address_advances_by_elements_times_width() {
    let base = 0x7f00_0000_0000i64;
    assert_eq!(typed_direct_view_address(base, 0, 4), base);
    assert_eq!(typed_direct_view_address(base, 1, 4), base + 4);
    assert_eq!(typed_direct_view_address(base, 2, 8), base + 16);
    assert_eq!(typed_direct_view_address(base, 5, 2), base + 10);
}

/// F26-1 §9.3's bound, restated for the typed widths — which make it
/// STRICTER, and that is the finding.
///
/// [`is_plausible_native_addr`]'s floor is 65 536 and a heap view's
/// `address` is `16 + offset * width`. A `byte` view needs an offset of
/// 65 520 to present an address the native-pointer screen ACCEPTS; a
/// `double` view needs only 8 190 — an 8 190-element array is ordinary.
/// The overlap is inert only because [`bb_state`] resolves the heap array
/// FIRST and an aliasing view always has one. This is a test rather than a
/// sentence so a future reordering of those two checks fails here instead
/// of dereferencing an array index as a pointer.
#[test]
fn typed_view_address_stays_below_the_native_pointer_floor() {
    assert!(!is_plausible_native_addr(typed_buffer_address(8_189, 8)));
    assert!(
        is_plausible_native_addr(typed_buffer_address(8_190, 8)),
        "a double view crosses the floor at 8_190 elements, not 65_520"
    );
    assert!(!is_plausible_native_addr(typed_buffer_address(65_519, 1)));
    assert!(
        is_plausible_native_addr(typed_buffer_address(65_520, 1)),
        "the byte bound F26-1 §9.3 recorded, unchanged"
    );
}

/// Saturating, not wrapping. A wrapped product can land as a large
/// POSITIVE value, which [`is_plausible_native_addr`] would accept as a
/// process pointer — so the overflow mode is a safety property here, not a
/// stylistic one.
///
/// `usize::MAX` is deliberately NOT the input: `usize::MAX as i64` is -1,
/// for which saturating and wrapping agree, so it cannot discriminate.
#[test]
fn a_pathological_typed_offset_saturates_rather_than_wrapping() {
    let big = (i64::MAX / 4) as usize;
    let a = typed_buffer_address(big, 8);
    assert_eq!(a, i64::MAX, "saturates at the top, not part way");
    assert!(typed_direct_view_address(1024, big, 8) > 0);
}

// =====================================================================
// F37-1 §5 — `FileChannel.open`'s option algebra.
//
// Every expectation is a pasted transcript from jdk-25.0.3+9
// (`scratchpad/f37/FcOpen.java`).
// =====================================================================

/// MEASURED: `FileChannel.open(p)` reads (`size()` answers 5 on a 5-byte
/// file) and its `write()` raises `NonWritableChannelException`.
#[test]
fn file_channel_open_with_no_options_is_a_read() {
    let m = file_channel_open_mode(&[]).unwrap();
    assert!(m.read);
    assert!(!m.write);
    assert!(!m.create, "and it does NOT create");
    assert!(!m.truncate);
}

/// The implied-READ rule is "no ACCESS option present", not "no options at
/// all": `open(p, CREATE)` is still a read. An implementation written as
/// `names.is_empty()` passes the row above and fails this one.
#[test]
fn a_non_access_option_alone_still_implies_read() {
    let m = file_channel_open_mode(&["CREATE"]).unwrap();
    assert!(m.read && !m.write && m.create);
}

/// **The row that stops the naive mapping onto
/// `FileDescriptorTable::open_write`**, which is
/// `.create(true).truncate(!append)` — it does BOTH unconditionally.
///
/// MEASURED: `open(p, WRITE)` followed by a one-byte write leaves the file
/// at its original 5 bytes, and `open(missing, WRITE)` raises
/// `java.nio.file.NoSuchFileException`. So WRITE alone neither truncates
/// nor creates.
#[test]
fn write_alone_neither_creates_nor_truncates() {
    let m = file_channel_open_mode(&["WRITE"]).unwrap();
    assert!(m.write);
    assert!(
        !m.create,
        "no CREATE ⇒ NoSuchFileException on a missing file"
    );
    assert!(!m.truncate, "no TRUNCATE_EXISTING ⇒ the contents survive");
    assert!(!m.append);
}

#[test]
fn append_implies_write_and_does_not_imply_read() {
    let m = file_channel_open_mode(&["APPEND"]).unwrap();
    assert!(m.append && m.write);
    assert!(!m.read);
}

#[test]
fn create_new_counts_as_create() {
    assert!(
        file_channel_open_mode(&["WRITE", "CREATE_NEW"])
            .unwrap()
            .create
    );
}

/// MEASURED, and the messages are compared EXACTLY:
///
/// ```text
/// open(p, READ, APPEND)              java.lang.IllegalArgumentException: READ + APPEND not allowed
/// open(p, APPEND, TRUNCATE_EXISTING) java.lang.IllegalArgumentException: APPEND + TRUNCATE_EXISTING not allowed
/// ```
///
/// A `matches!(…, IllegalArgumentException { .. })` assertion is blind to a
/// wrong message by construction — which is how
/// `"direct buffer has no backing array"` survived a prior repair of the
/// sibling buffer family. The swapped-messages mutant dies here and
/// nowhere else.
#[test]
fn the_two_illegal_option_combinations_carry_the_jdks_exact_messages() {
    assert_eq!(
        file_channel_open_mode(&["READ", "APPEND"]).unwrap_err(),
        "READ + APPEND not allowed"
    );
    assert_eq!(
        file_channel_open_mode(&["APPEND", "TRUNCATE_EXISTING"]).unwrap_err(),
        "APPEND + TRUNCATE_EXISTING not allowed"
    );
}

/// The two refusals are INDEPENDENT and order-free in the option array,
/// and the two legal near-misses are legal. An implementation that checked
/// only one refusal passes one row of the test above.
#[test]
fn the_open_option_refusals_are_independent_and_order_free() {
    assert!(file_channel_open_mode(&["APPEND", "READ"]).is_err());
    assert!(file_channel_open_mode(&["TRUNCATE_EXISTING", "APPEND"]).is_err());
    assert!(
        file_channel_open_mode(&["READ", "TRUNCATE_EXISTING"]).is_ok(),
        "READ + TRUNCATE_EXISTING is not one of the refusals"
    );
    assert!(
        file_channel_open_mode(&["WRITE", "APPEND"]).is_ok(),
        "WRITE + APPEND is legal; only READ + APPEND is not"
    );
}

/// Unrecognised options are IGNORED, not refused — the deliberate policy
/// difference from `parse_afc_open_options` next door, argued at
/// [`file_channel_open_mode`]. Refusing them would turn a working
/// `open(p, WRITE, SPARSE)` into a failure.
#[test]
fn unrecognised_open_options_are_ignored_rather_than_refused() {
    let m = file_channel_open_mode(&["WRITE", "SPARSE", "DSYNC", "DIRECT"]).unwrap();
    assert!(m.write && !m.create);
}

/// `Files.newOutputStream`'s default set — the common writable call, and
/// the one whose mapping onto `open_write(path, false)` is exact.
#[test]
fn the_output_stream_option_set_resolves_to_create_plus_truncate() {
    let m = file_channel_open_mode(&["WRITE", "CREATE", "TRUNCATE_EXISTING"]).unwrap();
    assert!(m.write && m.create && m.truncate && !m.append);
}

/// The CONTROL for [`is_plausible_native_addr`], and the reason a heap
/// buffer's `address` cannot be mistaken for a native pointer at any
/// offset a real buffer reaches: the screen's floor is the first mappable
/// page, so `16 + offset` stays refused until the offset alone exceeds
/// 64 KiB.
///
/// This is a KNOWN, BOUNDED gap and not a claim of safety: an aliasing view
/// at an offset of 65 520 or more presents an `address` the screen accepts.
/// It is inert today because `bb_storage_view` asks `bb_resolve_heap_array`
/// FIRST and only consults `address` when no `hb` resolves — and an
/// aliasing view always has an `hb`. Recorded here rather than in prose so
/// a future reordering of those two checks fails a test instead of
/// dereferencing an array index as a pointer.
#[test]
fn a_heap_view_address_stays_below_the_native_pointer_floor_for_ordinary_offsets() {
    assert!(!is_plausible_native_addr(heap_buffer_address(0)));
    assert!(!is_plausible_native_addr(heap_buffer_address(65_519)));
    assert!(
        is_plausible_native_addr(heap_buffer_address(65_520)),
        "the bound is real and this test states where it is; the heap arm \
         of `bb_storage_view` is what keeps it inert"
    );
}

/// `ByteBuffer.wrap(array, offset, length)`'s geometry, MEASURED:
/// `wrap(raw16, 4, 8)` is `pos/lim/cap = 4/12/16` with `arrayOffset() == 0`.
///
/// So `offset`/`length` move POSITION and LIMIT and the array-base offset
/// stays 0 — the opposite of `slice()`. A body that put the 4 into the
/// array-base `offset` would read the right bytes at position 0 and the
/// WRONG bytes at every absolute index, and would answer `arrayOffset() == 4`
/// where HotSpot answers 0.
#[test]
fn wrap_range_moves_position_and_limit_and_never_the_array_base_offset() {
    // The window `native_bb_wrap_range` builds, restated.
    let (arr_len, off, len) = (16i32, 4i32, 8i32);
    let w = BufferWindow {
        offset: 0,
        pos: off,
        lim: off + len,
        cap: arr_len,
        mark: -1,
    };
    assert_eq!((w.offset, w.pos, w.lim, w.cap), (0, 4, 12, 16));
    assert_ne!(
        w.offset,
        slice_window(0, off, arr_len).offset,
        "wrap(a,off,len) and slice() put the same number in DIFFERENT \
         places; conflating them is the mutant"
    );
}

#[test]
fn allocated_heap_bytebuffer_sets_real_address() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    assert_eq!(ctx.get_field_by_name(bb, "position"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(bb, "limit"), Value::Int(8));
    assert_eq!(ctx.get_field_by_name(bb, "capacity"), Value::Int(8));
    assert_eq!(ctx.get_field_by_name(bb, "mark"), Value::Int(-1));
    assert_eq!(ctx.get_field_by_name(bb, "address"), Value::Long(16));
}

// --- B1: bulk get/put destination/source bounds ---

#[test]
fn bb_get_bulk_rejects_negative_offset() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    let dst = ctx.new_array(ArrayElementType::Byte, 8);
    let r = native_bb_get_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(dst)),
            Value::Int(-1),
            Value::Int(4),
        ],
    );
    assert!(r.is_err(), "negative offset must throw, got {r:?}");
}

#[test]
fn bb_get_bulk_rejects_offset_plus_len_past_array() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    let dst = ctx.new_array(ArrayElementType::Byte, 4);
    // dst.len()==4, so off=2 len=4 overruns the destination array.
    let r = native_bb_get_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(dst)),
            Value::Int(2),
            Value::Int(4),
        ],
    );
    assert!(r.is_err(), "off+len past dst must throw, got {r:?}");
}

#[test]
fn bb_get_bulk_underflow_when_len_exceeds_remaining() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 2); // only 2 bytes remaining
    let dst = ctx.new_array(ArrayElementType::Byte, 16);
    let r = native_bb_get_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(8),
        ],
    );
    assert!(r.is_err(), "len>remaining must underflow, got {r:?}");
}

#[test]
fn bb_get_bulk_valid_copies_bytes() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 4);
    let view = bb_state(&ctx, bb).unwrap();
    for i in 0..4 {
        tb_write_elem(&mut ctx, view, i, Value::Int((i as i32) + 1)).unwrap();
    }
    let dst = ctx.new_array(ArrayElementType::Byte, 4);
    let r = native_bb_get_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(4),
        ],
    )
    .unwrap();
    assert!(r.is_some());
    for i in 0..4 {
        assert_eq!(ctx.get_array_element(dst, i), Value::Int((i as i32) + 1));
    }
}

#[test]
fn bb_get_bulk_reads_real_heap_layout_slot_hb() {
    let mut ctx = MockNativeContext::new();
    let arr = ctx.new_array(ArrayElementType::Byte, 8);
    for (i, b) in [9, 10, 11, 12].iter().enumerate() {
        ctx.set_array_element(arr, 2 + i, Value::Int(*b));
    }
    let bb = ctx.alloc_object(8);
    ctx.set_field(bb, 0, Value::Int(-1)); // mark
    ctx.set_field(bb, 1, Value::Int(1)); // position
    ctx.set_field(bb, 2, Value::Int(4)); // limit
    ctx.set_field(bb, 3, Value::Int(4)); // capacity
    ctx.set_field(bb, 5, Value::Object(Some(arr))); // ByteBuffer.hb
    ctx.set_field(bb, 6, Value::Int(2)); // ByteBuffer.offset

    let dst = ctx.new_array(ArrayElementType::Byte, 3);
    native_bb_get_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(3),
        ],
    )
    .unwrap();

    assert_eq!(ctx.get_array_element(dst, 0), Value::Int(10));
    assert_eq!(ctx.get_array_element(dst, 1), Value::Int(11));
    assert_eq!(ctx.get_array_element(dst, 2), Value::Int(12));
    assert_eq!(ctx.get_field(bb, 1), Value::Int(4));
}

/// W7-83, step 1 of 2: **the mock's own honesty, asserted before anything
/// depends on it.**
///
/// `MockNativeContext::heap_kind_of` returned `ObjectKind::Object`
/// unconditionally, `heap_element_type_of` returned
/// `ArrayElementType::Reference` unconditionally, and `object_is_array` was
/// left on the trait default `false` — for a mock that allocates arrays as
/// their own `HeapEntry::Array` variant and has always known the answer.
///
/// Every unit test in this crate runs against this mock, so those three
/// constants were not cosmetic: they made an entire species of screen
/// untestable here. A native that asks "is this actually an array?" got the
/// same answer for a `byte[]` and for an ordinary instance, so a test of
/// such a screen passed whether or not the screen was right — the probe
/// that cannot fail, one layer down from the code under test.
///
/// The assertions are paired (array AND non-array, each element type AND
/// its default value) precisely because a single-sided assertion is what a
/// constant satisfies.
#[test]
fn mock_answers_object_kind_from_its_own_heap_discriminant() {
    let mut ctx = MockNativeContext::new();
    let bytes = ctx.new_array(ArrayElementType::Byte, 4);
    let obj = ctx.alloc_object(3);

    assert_eq!(
        ctx.heap_kind_of(bytes),
        ObjectKind::Array,
        "the mock allocated this as HeapEntry::Array and must say so — a \
         constant `Object` here makes every array/instance fork in this \
         crate untestable"
    );
    assert_eq!(
        ctx.heap_kind_of(obj),
        ObjectKind::Object,
        "and it must still answer Object for an ordinary instance, or the \
         repair has merely inverted the constant"
    );
    assert!(ctx.object_is_array(bytes));
    assert!(!ctx.object_is_array(obj));

    // The element type is the argument `new_array` used to discard.
    assert_eq!(ctx.heap_element_type_of(bytes), ArrayElementType::Byte);
    let longs = ctx.new_array(ArrayElementType::Long, 2);
    assert_eq!(ctx.heap_element_type_of(longs), ArrayElementType::Long);
    assert_eq!(
        ctx.get_array_element(longs, 0),
        Value::Long(0),
        "a fresh long[] reads back Long(0), not Int(0) — the mock used to \
         fill every array with Int(0) whatever it was asked for"
    );
    let refs = ctx.new_ref_array(ClassId::new(0), 2);
    assert_eq!(ctx.heap_element_type_of(refs), ArrayElementType::Reference);
    assert_eq!(ctx.get_array_element(refs, 0), Value::Object(None));
    // The trait specifies `Reference` for a NON-array too, so this arm is
    // the contract rather than a leftover of the old constant.
    assert_eq!(ctx.heap_element_type_of(obj), ArrayElementType::Reference);
}

/// W7-83, step 2 of 2: **a `MemorySegment` at slot 5 is not a backing
/// array**, and the typed-view population that legitimately parks a real
/// array there still resolves.
///
/// Measured on HotSpot 25.0.3.9 (`probes/DirectByteBufferStateProbe.java`,
/// the `seg.*` section): `Arena.ofAuto().allocate(16).asByteBuffer()`
/// answers `hasArray() == false` and throws `UnsupportedOperationException`
/// from `array()`, and its `hb` is null while `Buffer.segment` holds a live
/// `jdk.internal.foreign.NativeMemorySegmentImpl`. `bb_resolve_heap_array`
/// used to return that segment and hand it to `heap_element_type_of` /
/// `get_array_element` as the backing array.
///
/// Both arms are in one test on purpose. The rejecting arm alone would pass
/// against a screen that refuses EVERYTHING at slot 5, which would take the
/// typed views out with it and turn W7-69's deliberate non-firing control
/// into a dead branch.
#[test]
fn bb_state_refuses_a_memory_segment_at_the_segment_slot() {
    let mut ctx = MockNativeContext::new();

    // --- the RED: a real-JDK-shaped receiver with a segment and no `hb`.
    let mut native = vec![31u8, 32, 33, 34];
    let addr = native.as_mut_ptr() as i64;
    let segment =
        ctx.alloc_object_with_class(0, "jdk/internal/foreign/NativeMemorySegmentImpl");
    let bb = ctx.alloc_object(8);
    ctx.set_field(bb, 0, Value::Int(-1)); // Buffer.mark
    ctx.set_field(bb, 1, Value::Int(0)); // Buffer.position
    ctx.set_field(bb, 2, Value::Int(4)); // Buffer.limit
    ctx.set_field(bb, 3, Value::Int(4)); // Buffer.capacity
    ctx.set_field(bb, 4, Value::Long(addr)); // Buffer.address
    ctx.set_field(bb, BB_SEGMENT_SLOT, Value::Object(Some(segment)));

    assert!(
        bb_resolve_heap_array(&ctx, bb).is_none(),
        "slot 5 is `java.nio.Buffer.segment`; a MemorySegment is not a \
         byte[] and must not be returned as one"
    );

    // And the receiver resolves as DIRECT, which is what it is — the
    // rejection degrades to the right answer rather than to "no storage".
    // `is_plausible_native_addr` is untouched and still guards this arm.
    let view = bb_state(&ctx, bb).expect("an Arena-backed buffer is a DIRECT buffer");
    match view.storage {
        BbStorage::Direct { addr: got } => assert_eq!(got, addr),
        BbStorage::Heap { arr, .. } => {
            panic!("resolved a heap array {arr:?} from a MemorySegment receiver")
        }
    }
    assert_eq!(tb_read_elem(&ctx, view, 0).unwrap(), Value::Int(31));

    // --- the CONTROL, in the same test: `native-builtins`' typed buffer
    // views park a REAL array at slot 5 because `segment` is the only
    // Object-typed field `Buffer` declares. That population must still
    // resolve, and W7-69 §3 relies on this arm staying live.
    let arr = ctx.new_array(ArrayElementType::Byte, 4);
    ctx.set_array_element(arr, 0, Value::Int(77));
    let view_buf = ctx.alloc_object(8);
    ctx.set_field(view_buf, 0, Value::Int(-1));
    ctx.set_field(view_buf, 1, Value::Int(0));
    ctx.set_field(view_buf, 2, Value::Int(4));
    ctx.set_field(view_buf, 3, Value::Int(4));
    ctx.set_field(view_buf, BB_SEGMENT_SLOT, Value::Object(Some(arr)));

    assert_eq!(
        bb_resolve_heap_array(&ctx, view_buf),
        Some(arr),
        "the screen must reject a non-array WITHOUT taking the typed-view \
         population with it"
    );
    let vv = bb_state(&ctx, view_buf).expect("a slot-5 array is backing storage");
    assert!(matches!(vv.storage, BbStorage::Heap { .. }));
    assert_eq!(tb_read_elem(&ctx, vv, 0).unwrap(), Value::Int(77));
}

#[test]
fn bb_get_bulk_reads_direct_buffer_address() {
    let mut ctx = MockNativeContext::new();
    let mut native = vec![21u8, 22, 23, 24];
    let bb = ctx.alloc_object(8);
    ctx.set_field(bb, 0, Value::Int(-1)); // mark
    ctx.set_field(bb, 1, Value::Int(1)); // position
    ctx.set_field(bb, 2, Value::Int(4)); // limit
    ctx.set_field(bb, 3, Value::Int(4)); // capacity
    ctx.set_field(bb, 4, Value::Long(native.as_mut_ptr() as i64)); // address

    let dst = ctx.new_array(ArrayElementType::Byte, 3);
    native_bb_get_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(3),
        ],
    )
    .unwrap();

    assert_eq!(ctx.get_array_element(dst, 0), Value::Int(22));
    assert_eq!(ctx.get_array_element(dst, 1), Value::Int(23));
    assert_eq!(ctx.get_array_element(dst, 2), Value::Int(24));
    assert_eq!(ctx.get_field(bb, 1), Value::Int(4));
}

#[test]
fn bb_put_bulk_rejects_negative_offset() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    let src = ctx.new_array(ArrayElementType::Byte, 8);
    let r = native_bb_put_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(src)),
            Value::Int(-2),
            Value::Int(4),
        ],
    );
    assert!(r.is_err(), "negative offset must throw, got {r:?}");
}

#[test]
fn bb_put_bulk_rejects_offset_plus_len_past_array() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 16);
    let src = ctx.new_array(ArrayElementType::Byte, 4);
    let r = native_bb_put_bulk(
        &mut ctx,
        &[
            Value::Object(Some(bb)),
            Value::Object(Some(src)),
            Value::Int(2),
            Value::Int(4),
        ],
    );
    assert!(r.is_err(), "off+len past src must throw, got {r:?}");
}

// --- B2: ByteArrayInputStream.read([BII) bounds ---

fn make_bais(ctx: &mut MockNativeContext, bytes: &[u8]) -> ObjectRef {
    let buf = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(buf, i, Value::Int(*b as i32));
    }
    let this = ctx.alloc_object_with_class(4, "java/io/ByteArrayInputStream");
    native_bais_init(ctx, &[Value::Object(Some(this)), Value::Object(Some(buf))])
        .expect("init ok");
    this
}

/// JDK order: EOF is reported before the zero-length short-circuit, so
/// `read(b, off, 0)` on a drained stream is -1. Regression guard for the
/// netty `testStreamTransfer1` failure.
#[test]
fn bais_read_zero_len_at_eof_is_minus_one() {
    let mut ctx = MockNativeContext::new();
    let this = make_bais(&mut ctx, b"hello");
    let dst = ctx.new_array(ArrayElementType::Byte, 8);
    let drain = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(5),
        ],
    )
    .expect("drain ok");
    assert_eq!(drain, Some(Value::Int(5)));
    let r = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(0),
        ],
    )
    .expect("zero-len read ok");
    assert_eq!(r, Some(Value::Int(-1)), "len=0 at EOF must report -1");
}

/// An EMPTY source is at EOF from the first call, so even the very first
/// zero-length read reports -1.
#[test]
fn bais_read_zero_len_empty_source_is_minus_one() {
    let mut ctx = MockNativeContext::new();
    let this = make_bais(&mut ctx, b"");
    let dst = ctx.new_array(ArrayElementType::Byte, 8);
    let r = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(0),
        ],
    )
    .expect("zero-len read ok");
    assert_eq!(r, Some(Value::Int(-1)));
}

/// ... but a zero-length read with bytes still available is 0, not -1.
#[test]
fn bais_read_zero_len_before_eof_is_zero() {
    let mut ctx = MockNativeContext::new();
    let this = make_bais(&mut ctx, b"hello");
    let dst = ctx.new_array(ArrayElementType::Byte, 8);
    let r = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(0),
        ],
    )
    .expect("zero-len read ok");
    assert_eq!(r, Some(Value::Int(0)));
}

#[test]
fn bais_read_bytes_rejects_negative_off() {
    let mut ctx = MockNativeContext::new();
    let this = make_bais(&mut ctx, b"hello");
    let dst = ctx.new_array(ArrayElementType::Byte, 8);
    let r = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(-1),
            Value::Int(2),
        ],
    );
    assert!(r.is_err(), "negative off must throw, got {r:?}");
}

#[test]
fn bais_read_bytes_rejects_off_plus_len_past_array() {
    let mut ctx = MockNativeContext::new();
    let this = make_bais(&mut ctx, b"hello");
    let dst = ctx.new_array(ArrayElementType::Byte, 4);
    let r = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(3),
            Value::Int(4),
        ],
    );
    assert!(r.is_err(), "off+len past dst must throw, got {r:?}");
}

#[test]
fn bais_read_bytes_valid_in_bounds_ok() {
    let mut ctx = MockNativeContext::new();
    let this = make_bais(&mut ctx, b"hello");
    let dst = ctx.new_array(ArrayElementType::Byte, 8);
    let r = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(1),
            Value::Int(5),
        ],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(5)));
}

// --- B3: typed-buffer absolute accessor lower-bound + overflow ---

#[test]
fn bb_get_int_abs_rejects_negative_index() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    let r = native_bb_get_int_abs(&mut ctx, &[Value::Object(Some(bb)), Value::Int(-1)]);
    assert!(r.is_err(), "negative index must throw, got {r:?}");
}

#[test]
fn bb_get_int_abs_rejects_index_overflow() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    // i32::MAX-2 + 4 overflows; must be rejected, not panic / pass.
    let r = native_bb_get_int_abs(
        &mut ctx,
        &[Value::Object(Some(bb)), Value::Int(i32::MAX - 2)],
    );
    assert!(r.is_err(), "overflowing index must throw, got {r:?}");
}

#[test]
fn bb_put_int_abs_rejects_negative_index() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    let r = native_bb_put_int_abs(
        &mut ctx,
        &[Value::Object(Some(bb)), Value::Int(-1), Value::Int(0x1234)],
    );
    assert!(r.is_err(), "negative index must throw, got {r:?}");
}

#[test]
fn bb_get_int_abs_valid_index_ok() {
    let mut ctx = MockNativeContext::new();
    let bb = make_bb(&mut ctx, 8);
    let r = native_bb_get_int_abs(&mut ctx, &[Value::Object(Some(bb)), Value::Int(4)]);
    assert!(r.is_ok(), "in-range index must succeed, got {r:?}");
}

#[test]
fn tb_get_int_abs_rejects_negative_index() {
    let mut ctx = MockNativeContext::new();
    // IntBuffer-style typed buffer: cap=4 elements.
    let buf = alloc_typed_buffer(&mut ctx, "java/nio/IntBuffer", ArrayElementType::Int, 4);
    buf_set_limit(&mut ctx, buf, 4);
    let r = native_tb_get_int_abs(&mut ctx, &[Value::Object(Some(buf)), Value::Int(-1)]);
    assert!(r.is_err(), "negative element index must throw, got {r:?}");
}

#[test]
fn tb_get_int_abs_rejects_index_at_capacity() {
    let mut ctx = MockNativeContext::new();
    let buf = alloc_typed_buffer(&mut ctx, "java/nio/IntBuffer", ArrayElementType::Int, 4);
    buf_set_limit(&mut ctx, buf, 4);
    // idx == cap is out of range (valid indices are 0..cap).
    let r = native_tb_get_int_abs(&mut ctx, &[Value::Object(Some(buf)), Value::Int(4)]);
    assert!(r.is_err(), "index==cap must throw, got {r:?}");
}
