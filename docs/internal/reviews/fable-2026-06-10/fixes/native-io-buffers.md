# Fix note — `native-io-buffers`

Reviewer findings addressed: B1, B2, B3 from
`docs/reviews/fable-2026-06-10/native-io.md`.

Owned files edited:
- `native-io/src/lib.rs`
- (`native-io/src/direct_buffer.rs` — inspected, **no change needed**; see below)

All three findings are wrong-result / debug-panic bugs (not memory-unsafety —
the GC layer's `set_array_element`/`get_array_element` are bounds-checked and
silently no-op on OOB). The fixes make the natives throw the JDK-mandated
exception up front instead of silently dropping the write (release) or
overflowing an index add (`overflow-checks` debug build → panic).

---

## B1 — `ByteBuffer` bulk get/put skip destination-bounds + negative/overflow checks

`native_bb_get_bulk` (`lib.rs:5359`) and `native_bb_put_bulk` (`lib.rs:5436`).

- `offset`/`length` were read as `*v as usize`, which silently lost the sign
  (a negative Java `offset` became a huge `usize`). The only guard was
  `length > remaining` against the *buffer*, never against the array.
- Fix: read `offset`/`length` as `i32`, then call the existing
  `check_array_bounds(offset, length, ctx.array_length(dst|src))` BEFORE
  touching the buffer — exactly the JDK order (`Objects.checkFromIndexSize`
  on the array first, then the buffer's `remaining`). `check_array_bounds`
  rejects negative `off`/`len` and uses `checked_add` for the upper bound, so
  no overflow path remains. After the check the values are shadowed to `usize`.
- The buffer-space check now throws the dedicated
  `RuntimeError::BufferUnderflowException` / `BufferOverflowException`
  variants (these already exist and map to the correct
  `java.nio.Buffer{Under,Over}flowException` — the previous code wrapped the
  text in `IllegalStateException`, which real `catch (BufferUnderflowException)`
  blocks miss). `check_array_bounds` raises
  `ArrayIndexOutOfBoundsException`, a subclass of `IndexOutOfBoundsException`,
  so JDK `catch (IndexOutOfBoundsException)` still catches it.

## B2 — `ByteArrayInputStream.read([BII)` skips off/len bounds check

`native_bais_read_bytes` (`lib.rs:2428`).

- `off`/`len` were `*v as usize` with no validation against `buf.length`,
  unlike the sibling `native_fis_read_bytes`. Both the subclass virtual-dispatch
  fallback loop and the BAIS fast path could index out of range / overflow
  `off + i`.
- Fix: read `off`/`len` as `i32` and call
  `check_array_bounds(off, len, ctx.array_length(buf))` immediately after
  parsing the args — i.e. before either path runs (mirrors the JDK's
  `Objects.checkFromIndexSize(off, len, b.length)` at the top of
  `InputStream.read(byte[],int,int)`). The `len == 0` / `off == b.length`
  corner is permitted (checked-add `off + 0 <= len`), matching the JDK.

## B3 — typed-buffer absolute accessors: lower-bound + overflow

Two distinct accessor shapes, both fixed; two small helpers added next to
`bb_state` (`lib.rs:~4660`):

- `abs_access_in_bounds(index, width, bound)` — for the **byte-offset** typed
  accessors `native_bb_get_int_abs` / `native_bb_put_int_abs`, whose guard was
  `if index + 4 > cap`. That passed a negative `index` (e.g. `-1 + 4 = 3 <= cap`)
  and could overflow `index + 4` for `index` near `i32::MAX` (debug panic).
  Replaced with `!abs_access_in_bounds(index, 4, cap)` = `index >= 0 &&
  index.checked_add(4) <= cap`. (These are the only byte-offset typed absolute
  accessors on the heap ByteBuffer — there are no `*_long_abs` / `*_short_abs`
  byte-offset variants.)

- `tb_index_in_bounds(idx, bound)` = `idx >= 0 && idx < bound` — for the
  **element-indexed** typed-buffer absolute accessors
  (`native_cb_get_abs`/`put_abs` and the `native_tb_{get,put}_{int,long,float,
  double,short}_abs` family). These previously did `idx = *v as usize` with
  **no** bounds check at all, so a negative Java index became a huge `usize`
  (silent no-op at the GC layer). They now read `idx` as `i32`, validate against
  `cap`, and throw on OOB, matching the single-byte `native_bb_get_abs`/`put_abs`
  which already checked `index < 0 || index >= cap`.

Note on the element-indexed check: the strict JDK checks the element index
against the buffer's *limit*; these synthetic heap typed buffers are allocated
with `limit == capacity`, and validating against `cap` is the upper bound that
keeps the backing array from being over-indexed (the B3 concern). This matches
the pre-existing `native_bb_get_abs`/`put_abs` convention in this file.

Exception type for the abs accessors is kept as
`IllegalArgumentException { message: "IndexOutOfBoundsException" }` to match the
established pattern already used by `native_bb_get_int_abs`/`get_abs` in this
file (not changed to avoid an unrelated cross-cutting refactor of how these
natives surface IOOBE).

---

## `direct_buffer.rs` — inspected, no change

The B1–B3 family lives entirely in `lib.rs`. `direct_buffer.rs` only handles
`Unsafe.{allocate,free}Memory` for the off-heap arena; the *direct*-buffer bulk
get/put route through `ctx.copy_to/from_native_memory` (already
address-validated per the review summary), so there is no analogous unchecked
bulk/abs native there.

## Tests

Added `#[cfg(test)] mod buffer_bounds_tests` in `lib.rs` (after
`bais_layout_tests`), using the existing `MockNativeContext`:

- B1: `bb_get_bulk` / `bb_put_bulk` — negative offset, `off+len` past the
  array, `len > remaining` underflow, plus a valid round-trip that copies bytes.
- B2: `bais_read_bytes` — negative `off`, `off+len` past the array, plus a
  valid in-range read returning the byte count.
- B3: `bb_get_int_abs` — negative index and `i32::MAX-2` overflow rejected,
  valid index ok; `bb_put_int_abs` negative index rejected; `tb_get_int_abs`
  (IntBuffer) negative index and `idx == cap` rejected.

## Build / risk

Did not run cargo (per instructions). Edits mirror the existing native idioms
exactly (same `RuntimeError::*.into()` error path, same `bb_state`/
`check_array_bounds`/`ctx.array_length` helpers, same `MockNativeContext` test
API as the adjacent BAIS suite). Behavioral risk: low — the new checks only
fire on inputs that were previously silently dropped (release) or panicked
(debug); valid in-range calls are unchanged. The buffer-space error type change
(IllegalState→Buffer{Under,Over}flow) is strictly more JDK-correct and is what
real `catch` blocks expect.
