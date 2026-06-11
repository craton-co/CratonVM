# Fix note — native-awt-overflow

Findings B1, B2, B4 (bugs) and V1 (vulnerability) from
`docs/reviews/fable-2026-06-10/native-awt.md`. All four are
untrusted-input-driven unbounded-allocation / integer-overflow defects in the
headless software rasterizer, reachable directly from Java draw calls. All are
real bugs (not behavior fakes), so each is actually fixed — no `app-stubs`
gating applies.

## B1 (HIGH) — `fillArc`/`drawArc` unbounded `steps` → OOM panic / hang

### Finding
`renderer.rs` `draw_arc`/`fill_arc` computed
`steps = ((rx.max(ry) as f32 * arc_angle.abs() / 90.0).ceil() as usize).max(16)`.
`rx`/`ry` derive from Java arc width/height (`graphics2d.rs:387-403`, bounded
only by `w/h < 0` reject → up to ~1.07e9), and `arc_angle` is the Java `extent`.
`Graphics.fillArc(0,0,Integer.MAX_VALUE,Integer.MAX_VALUE,0,360)` made
`steps ≈ 4e9` → `fill_arc` did `Vec::with_capacity(~4e9)` (~32 GB → abort);
`draw_arc` looped ~4e9 times (effective hang). A NaN/inf `arc_angle` saturated
`... as usize` to `usize::MAX` → instant `with_capacity` panic.

### Root cause
Step count was an unclamped function of two untrusted inputs (radius and sweep),
with no buffer-extent bound and no finite-angle sanitization.

### Exact change (`renderer.rs`)
- Added module const `MAX_ARC_STEPS = 1 << 16`.
- Added private `SoftwareRenderer::arc_steps(rx, ry, arc_angle)` that:
  sanitizes `arc_angle` to a finite value clamped to `360.0`; clamps the
  effective radius to `max(width, height)` (no on-screen arc resolves finer
  than its pixel radius); then `.clamp(16, MAX_ARC_STEPS)`. Because the angle is
  finite and the radius bounded, the `as usize` cast is well-defined (no
  saturation).
- `draw_arc` and `fill_arc` now call `self.arc_steps(...)` instead of the inline
  formula. Normal arcs (e.g. the existing `test_draw_arc`/`test_fill_arc`,
  rx=ry=10, 90°) still yield 16 steps — byte-identical behavior; only the
  pathological tail is clamped.

## B2 (HIGH) — `copy_area` fallback `w * h` u32-overflow panic

### Finding
`renderer.rs` `copy_area` out-of-bounds-source fallback did
`Vec::with_capacity((w * h) as usize)` where `w`/`h` are `u32` straight from
Java `copyArea` (up to ~2.1e9 each, `natives.rs` `get_int(...).max(0) as u32`).
`w * h` is a **u32** multiply: panics on overflow in debug, wraps in release
(under-sizing the temp → realloc). Java fully controls reaching this branch
(`x < 0` or `x + w > width`).

### Root cause
Capacity computed with a u32 multiply on caller-controlled extents.

### Exact change (`renderer.rs`)
- Compute the span as `(w as usize).checked_mul(h as usize)`; on overflow,
  `return` (bail) instead of panicking, mirroring the image-path discipline.
- Also hardened the temp-buffer **read index** in the write-back loop from
  `(sy as u32 * w + sx as u32) as usize` to `sy as usize * w as usize + sx as usize`:
  `sy * w` can exceed u32 range even when `w * h` fits in `usize`. (The
  `self.pixels` index above it is bounded by the real buffer size, which
  `safe_buffer_dims` already guarantees fits in u32, so it was left as-is.)

## B4 (LOW) — `draw_rect` stroke arithmetic i32 overflow

### Finding
`renderer.rs` `draw_rect` used `sw = (self.stroke_width as i32).max(1)` then
`h - 2 * sw as u32` / `2 * sw`. `stroke_width: f32` is Java-controlled
(`Graphics2D.setStroke`; `set_stroke_width` stores it raw). A huge value made
`2 * sw` overflow i32 (panic in debug) before the `h as i32 > 2 * sw` guard was
meaningful; NaN/negative widths were also unhandled.

### Root cause
`stroke_width` cast to i32 and doubled without sanitization or an upper bound.

### Exact change (`renderer.rs`)
- Clamp `sw` to `[1, min(w, h, i32::MAX/2)]` (a stroke can't exceed the shape it
  outlines; the `i32::MAX/2` ceiling guarantees `2 * sw` can't overflow i32).
- NaN/inf `stroke_width` falls back to `sw = 1` via `is_finite()`.
- Hoisted `inner_h = h - 2 * sw as u32` to a local (the guard ensures it can't
  underflow). Normal tiny strokes are unaffected.

## V1 (MEDIUM) — `read_int_array` trusts `array_length` for `with_capacity`

### Finding
`natives.rs` `read_int_array` did `Vec::with_capacity(ctx.array_length(obj))`.
A malformed/hostile array object reporting an absurd length forces a giant
eager up-front allocation before any element is read.

### Root cause
The reported length is used directly as a reserve hint with no cap.

### Exact change (`natives.rs`)
- Added const `MAX_INT_ARRAY_PREALLOC = 1 << 20` (1M ints / 4 MiB).
- Reserve `len.min(MAX_INT_ARRAY_PREALLOC)`; the loop still appends exactly
  `len` elements, so legitimately large valid arrays still work — the `Vec`
  just grows on demand instead of pre-reserving from an untrusted length.

## Files touched
- `native-awt/src/renderer.rs` — B1 (`MAX_ARC_STEPS`, `arc_steps`, `draw_arc`,
  `fill_arc`), B2 (`copy_area` fallback), B4 (`draw_rect`); 5 new `#[cfg(test)]`
  tests.
- `native-awt/src/natives.rs` — V1 (`MAX_INT_ARRAY_PREALLOC`, `read_int_array`);
  1 new `#[cfg(test)]` test.
- `native-awt/src/graphics2d.rs` — **not modified.** B1 cites the arc-dimension
  source here, but the correct clamp point is the renderer, which is the only
  layer that knows the buffer extent. `draw_arc`/`fill_arc` in graphics2d.rs
  already reject `w/h < 0` and need no change.
- `docs/reviews/fable-2026-06-10/fixes/native-awt-overflow.md` — this note.

## Tests added
- `test_arc_steps_clamped_to_max` — `arc_steps` bounded for huge radii and
  out-of-range / NaN / inf sweeps; floor of 16 preserved.
- `test_fill_arc_extreme_dims_no_panic` — `fillArc` with `i32::MAX/2` dims +
  NaN sweep completes without OOM/panic (B1, allocating path).
- `test_draw_arc_extreme_dims_no_hang` — `drawArc` with `i32::MAX/2` dims is
  bounded (B1, loop path).
- `test_copy_area_overflow_dims_no_panic` — `copyArea` fallback with
  overflow-product extents returns gracefully (B2).
- `test_draw_rect_huge_stroke_no_overflow` — `i32::MAX` / NaN / negative stroke
  widths don't overflow (B4).
- `int_array_prealloc_cap_is_sane` — documents/guards the V1 reserve cap.

Note on V1 test scope: there is no mock `NativeContext` in the `natives.rs` test
module (the existing tests exercise event synthesis without one). Building a
fabricated `NativeContext` that returns a huge `array_length` would require
re-implementing the full trait and keeping it in sync — fragile for a one-line
defensive cap. The added pure-logic test verifies the cap relationship instead.

## Follow-up & risk
- Behavior-neutral for all normal inputs: arc step counts, copy_area in-bounds
  fast paths, and small stroke widths are unchanged; only pathological /
  untrusted-extreme inputs now clamp instead of panicking/hanging.
- Related findings left for their owners (out of my file scope): B3 / P4
  (`fill_polygon` scanline spans full vertex Y range — same unbounded-work class,
  also in `renderer.rs` but called out as a separate finding) is **not** fixed
  here; it should be clamped to `[0, height) ∩ clip` in `fill_polygon`. V2
  (`bilinear/bicubic` short-slice precondition) is latent (no current short-slice
  caller) and untouched.
- A shared "clamp rect/extent to buffer ∩ clip" helper (Feature Suggestion 3 in
  the report) would consolidate B1/B3/P4; deferred to keep this change surgical.
