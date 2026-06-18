# Fix note — native-awt-polygon (B3 / P4)

**Agent id:** `native-awt-polygon`
**Owned file:** `native-awt/src/renderer.rs`
**Bug:** B3 (low) / P4 — `fill_polygon` scanline loop spans the full vertex Y
range and the X spans run the full intersection width, regardless of buffer
size. A polygon with vertices near `i32::MIN`/`i32::MAX` (reachable directly
from Java via `Graphics.fillPolygon`, and indirectly via `fillArc` which
synthesises a polygon) drives ~4.3e9 scanline iterations — an unbounded-work
hang from untrusted input — even though every pixel write is already clipped by
`put_pixel`. Carry-over from Round 4 (`native-awt-overflow`, which fixed the
sibling allocation bugs B1/B2).

## Root cause

`fill_polygon` (renderer.rs ~1141) iterated `for y in min_y..=max_y` where
`min_y`/`max_y` are raw polygon vertex Y values, and the inner span loop ran
`for x in x_start..=x_end` over raw edge-intersection X values. Both ranges are
fully attacker-controlled and unbounded; only `put_pixel`'s per-pixel
`pixel_visible` check (`x<0 || y<0 || x>=width || y>=height` plus clip) bounded
the *writes* — not the *iteration count*. So work was O(vertex-coordinate-range),
not O(surface-area).

## Fix

Clamp the scanline `y` iteration to `[0, height)` and each fill span `x` to
`[0, width)`, both intersected with the active clip rect — **but only when the
transform is identity**. Two-part change inside `fill_polygon`:

1. Before the y-loop, compute `scan_y0/scan_y1` and `span_x_lo/span_x_hi`:
   - **Identity transform:** `y_lo = min_y.max(0)`,
     `y_hi = max_y.min(height-1)`, `x_lo/x_hi = [0, width-1]`, each further
     intersected with the clip rect's extent (`clip.y + clip.height - 1`, etc.,
     matching `Rect::contains` semantics). Loop becomes `scan_y0..=scan_y1`.
   - **Non-identity transform:** keep the full `(min_y, max_y)` range and
     `None` span bounds (no clamp).
2. Inside the span fill, when identity, clamp `x_start = x_start.max(x_lo)` /
   `x_end = x_end.min(x_hi)`. A resulting `x_start > x_end` yields an empty
   inclusive range (no-op), exactly matching the pixels `put_pixel` would have
   dropped.

## Why this is byte-identical for in-bounds polygons

When the transform is identity, `tx(x, y)` is `(round(x as f64), round(y as f64))`.
Because `x`/`y` here are `i32` values cast to `f64`, `round` is exact and returns
the same integer, so `put_pixel` receives exactly `(x, y)`. Therefore any
scanline `y` outside `[0, height) ∩ clip` or column `x` outside
`[0, width) ∩ clip` produces **zero** visible pixels under the old code — the
clamp removes only iterations that were already no-ops. Output is unchanged.

The clamp is deliberately **gated on `self.transform.is_identity()`**: under a
non-identity affine transform a logical coordinate outside the surface can map
*back into* the visible buffer, so clamping in logical space would be incorrect.
That path keeps the original (unbounded but correct) range — matching the
existing convention where the renderer only takes surface-clamp fast paths in the
identity case (cf. `blit_image` at renderer.rs ~1211).

Edge cases checked:
- `width`/`height` == 0 → `as i32 - 1 == -1` → empty range, no underflow panic,
  draws nothing (correct for a zero-size surface).
- `f64::ceil()/floor() as i32` saturate, so `x_start`/`x_end` are finite i32
  before clamping; no cast panic.
- `>2^31` buffer dims use the same `as i32` convention already used crate-wide
  (lines 707, 1227–1228); not introduced here. B3 is about extreme *vertex*
  coords, not buffer dims (those are bounded by allocation reality).

## Tests added (`#[cfg(test)]`, renderer.rs)

- `test_fill_polygon_extreme_vertices_no_hang` — a triangle with `i32::MIN/MAX`
  vertices on a 16×16 surface must complete immediately (would hang the suite
  without the clamp).
- `test_fill_polygon_clamp_is_byte_identical` — an in-bounds triangle renders
  identically and its centroid stays filled (clamp is behaviour-neutral).

Mirrors the existing B1/B2 sibling tests (`test_fill_arc_extreme_dims_no_panic`,
`test_copy_area_overflow_dims_no_panic`).

## Policy / scope

No synthetic-stub concerns — pure rasterizer hardening. No new feature flags. No
non-owned files touched. Only `native-awt/src/renderer.rs` edited.

## Build note

Did not run cargo/git per instructions. Edits mirror existing types/APIs
(`Rect{x,y,width,height}`, `self.clip: Option<Rect>`, `is_identity()`,
`set_color`/`pixels()` used in sibling tests). High confidence it compiles.
