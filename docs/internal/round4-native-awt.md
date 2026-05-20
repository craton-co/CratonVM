# Round 4 — `native-awt` perf/correctness review

Scope: `native-awt/src/**` after rounds 1–3 landed (EDT Runnable dispatch,
GlyphAtlas singleton, fontdue top-level dep, Cocoa fontdue rasterizer).
Findings sorted by expected impact. Known deferred item (migrate X11 +
Cocoa `rasterize_text` to use `global_glyph_atlas()`) is NOT re-listed
here — but [HIGH-1] below identifies a hard blocker that must be
addressed before that migration is meaningful on Linux.

---

## [HIGH-1] X11 reloads the system TTF from disk on every `rasterize_text` call
**File:** `native-awt/src/platform/x11.rs:716-741` (`load_fontdue_font`)

`load_fontdue_font` does `std::fs::read(path)` followed by
`fontdue::Font::from_bytes(...)` **on every call**. Cocoa's sibling
function (`platform/cocoa.rs:723-765`) wraps the result in a
`static OnceLock<Option<fontdue::Font>>` and only pays this cost once;
the X11 version does not. Every text-drawing operation on Linux opens
`/usr/share/fonts/.../DejaVuSans.ttf`, reads ~700 KB into a fresh `Vec<u8>`,
and re-runs fontdue's OpenType parser before rasterizing a single glyph.

This is a strict *blocker* for the deferred glyph-atlas migration: even
after the atlas caches the rasterized bitmaps, `rasterize_text` must still
acquire a `fontdue::Font` to hand to `get_or_rasterize`, and that
acquisition will keep paying the disk-read + parse cost per call. The
atlas can only help if the underlying `Font` is itself cached.

**Fix:** Mirror Cocoa exactly — wrap the search-and-parse loop in a
`static FONT: OnceLock<Option<fontdue::Font>>`, return `.clone()` from the
cached `Option`. Two-line change. Removes ~99% of CPU on Linux text-heavy
workloads.

---

## [HIGH-2] Win32 `rasterize_text` rebuilds DirectWrite factory + GDI HFONT/HBITMAP every call
**File:** `native-awt/src/platform/win32.rs:580-753`

`Win32Backend` carries a `DirectWriteRenderer` with a `text_format_cache`
(line 42) explicitly for this purpose — but the cache is never used:
`rasterize_text` is `&self` (line 580), so it can't reach
`self.dwrite.as_mut()`, so it allocates a brand-new `IDWriteFactory` plus
a fresh `IDWriteTextFormat` and a fresh `HFONT`/`HBITMAP`/`HDC` per call
(lines 597, 619, 660–718). The `text_format_cache` is dead code today.

The cost is two trips through DirectWrite COM init (`DWriteCreateFactory`,
`CreateTextFormat`) plus `CreateCompatibleDC`/`CreateDIBSection`/
`CreateFontW` GDI handles **per glyph batch** — a JTextField repainting
on every keystroke leaks thousands of font-format COM objects through GC
pressure and burns substantial CPU.

**Fix:** Move the GDI/DWrite caches behind `RefCell` or `Mutex` (the
trait already takes `&self`, so interior mutability is required), or
widen the trait to `&mut self` for `rasterize_text` (the renderer call
site on the EDT can satisfy that). Then the existing `text_format_cache`
becomes effective and `CreateFontW`/`CreateDIBSection` can be keyed too.

---

## [HIGH-3] `with_gfx` serialises every Graphics2D draw call through one global mutex
**File:** `native-awt/src/natives.rs:97-118` (and call sites lines 512–862)

Every `drawLine`/`fillRect`/`drawString`/etc. native takes
`gfx_registry().lock()` — a single process-wide `parking_lot::Mutex<HashMap<i32, GfxEntry>>`
— for the *entire duration* of the rasterizer call inside the closure
(`f(&mut entry.state)`). Two visible costs:

1. **Hot-path contention** between the EDT painter and any background
   thread that touches a Graphics2D (offscreen `BufferedImage` rendering
   in worker threads is legal in AWT). The whole rasterizer (`fill_rect`,
   `blit_image`, `draw_string`) runs under the lock.
2. **`std::collections::HashMap`** with the default SipHash random state
   is used for an `i32` identity-hash key (line 79), redundantly slow
   compared to the `FxHashMap` already used elsewhere in this file.

**Fix:** Two-step. (a) Swap the inner map to `FxHashMap<i32, GfxEntry>`.
(b) Restructure so the lock only covers the `get_mut` + take-out-of-map
phase: pull the `GfxEntry` out (or wrap each entry in its own
`Arc<Mutex<Graphics2DState>>`), drop the outer lock, then call `f`
against the per-entry lock. Per-context locking eliminates cross-context
contention entirely.

---

## [HIGH-4] `drawImage` native copies the entire source pixel buffer on every blit
**File:** `native-awt/src/natives.rs:610-631`

The `drawImage(Image, x, y, observer)` native does
`bimg.get_data_buffer().to_vec()` (line 619) inside the
`image_registry()` lock, then drops the lock and hands the `Vec<u32>` to
`g.draw_image(&pixels, ...)`. The `to_vec()` is a full copy of the
source image's pixels on every draw call — for a typical 256×256 icon
that is 256 KiB of memcpy per draw, for a 1080p backing image it's 8 MiB.

`SoftwareRenderer::blit_image` is `&mut self` for the destination and
takes `src: &[u32]` for the source — the lock-juggling here is the only
reason for the copy. Worse, the `dispose_gfx` flush path (`natives.rs:171`)
does the same `to_vec()` for the destination.

**Fix:** Either (a) hold the `image_registry` lock across the blit (the
blit doesn't recursively need it), or (b) wrap `BufferedImageData.pixels`
in `Arc<[u32]>` (write-on-clone) so callers can cheaply clone the `Arc`
and release the lock. Option (b) also fixes the `dispose_gfx` copy.

---

## [HIGH-5] `Graphics2DState::fill_rect_gradient` uses per-pixel `set_color` + `draw_pixel`
**File:** `native-awt/src/graphics2d.rs:573-589`

The gradient fill path iterates `w * h` pixels and for *each* pixel:
1. Calls `gradient_color_at()` (already cheap),
2. Calls `self.renderer.set_color(c)` (writes through `&mut SoftwareRenderer`),
3. Calls `self.renderer.draw_pixel(x+px, y+py)` which transforms,
   bounds-checks, clips, and runs the full `composite()` Porter-Duff
   blend (`renderer.rs:638-644`).

For a 256×256 gradient that is 65 536 individual virtual calls into the
renderer plus 65 536 SRC_OVER composites. The SSE2 `composite_row_*`
helpers in `renderer.rs:339-532` are designed for exactly this row-at-a-time
shape but aren't reached.

**Fix:** Compute one full row of gradient colours into a scratch
`Vec<u32>`, then call `SoftwareRenderer::blit_image` for that row (or add
a dedicated `fill_rect_gradient_row` helper on the renderer). Each row
then dispatches through `composite_row_src_over` and benefits from SSE2.
~10–50× speedup expected for solid-alpha gradients.

---

## [HIGH-6] `invocation_event_callbacks` side-table has unbounded growth on un-dispatched events
**File:** `native-awt/src/natives.rs:39-50, 970-994` (allocation) and `1004-1033` (drain)

`EventQueue.getNextEvent` allocates an `InvocationEvent` and inserts
`(java_identity_hash, callback_id)` into `invocation_event_callbacks`
(line 983). The entry is only removed by
`InvocationEvent.dispatch()V` calling
`take_invocation_event_callback` (line 1008). If `dispatch()V` is not
called — common scenarios: the Java EDT discards the event, the event
flows through a custom `EventQueue.dispatchEvent` that doesn't call
`dispatch()` on invocation events, or app code calls `getNextEvent`
twice without dispatching the first — the entry leaks. Combined with
the parallel runnable leak in `EventDispatchThread.runnables`
(`edt.rs:106`, only cleared on `stop()` lines 181–182), each undispatched
`invokeLater` permanently consumes one `(i32, u64)` + one `(u64, ObjectRef)`
plus the `Arc<Runnable>` chain that pins the Java object alive.

The task statement specifically flags this side-table as a known concern.

**Fix:** (a) Cap `invocation_event_callbacks` at a few thousand entries
and evict FIFO on insert when full (matches the GlyphAtlas pattern at
`font.rs:444-446`). (b) Add a periodic sweep that drops bindings whose
callback_id is older than N seconds (use a `(u64, Instant)` value). (c)
Best: register a Java weak-ref / phantom-ref against the `InvocationEvent`
so the binding goes away when the Java object is collected. Same fix
shape applies to `EventDispatchThread.runnables`.

---

## [MED-7] `FontMetrics` natives read `args[0]` (`this`) as if it were the font size
**File:** `native-awt/src/natives.rs:1061-1084`

Every `FontMetrics` native does `get_int(args, 0).max(12)` and treats
the result as the font size:

```rust
registry.register("java/awt/FontMetrics", "getAscent", "()I", |_ctx, args| {
    int_ok(((get_int(args, 0).max(12) as f32) * 0.8).round() as i32)
});
```

But `args[0]` is the receiver `this` (an `ObjectRef` packed via
`get_int` returns 0 for the Object variant — see `get_int` at line 241).
Result: every call returns `(12 * factor).round()` because of the
`.max(12)` floor, regardless of the actual font. All six metric methods
(`getAscent`/`getDescent`/`getLeading`/`getHeight`/`getMaxAdvance`/
`charWidth`) and `stringWidth` (line 1074) are affected.

This is a correctness bug visible to any Swing layout: every component
believes its font is exactly 12 pt, so text wraps wrong, JLabel
preferred-sizes are wrong, table column widths are wrong.

**Fix:** Read the actual font triple from the receiver:
`(family, style, size) = (ctx.read_string(this) for name field, ctx.get_field_by_name(this, "size") as int, ...)`,
then delegate to `crate::font::font_engine().get_metrics(&FontSpec::...)`.
The proper computation already exists in `font.rs:268-288` and is
unused by the natives layer.

---

## [MED-8] `RenderingHints` setter unconditionally turns antialiasing ON for every hint key
**File:** `native-awt/src/natives.rs:760-778`

The body reads `key_hash` and `val_hash`, immediately throws them away
(`let _ = (key_hash, val_hash);`), then calls
`g.set_rendering_hint(K::Antialiasing, V::On)` regardless of what the
caller actually requested. So `setRenderingHint(KEY_INTERPOLATION,
VALUE_INTERPOLATION_NEAREST_NEIGHBOR)` turns AA on; trying to disable
AA via `setRenderingHint(KEY_ANTIALIASING, VALUE_ANTIALIAS_OFF)` also
turns it on. The existing TODO at line 772 acknowledges this.

Visible impact: every app pays the AA path (Wu's line at
`renderer.rs:1315`, scanline coverage in `put_pixel_aa`) even when it
explicitly opted out, doubling rendering cost for line-heavy charts.

**Fix:** Add a small `HashMap<i32, RenderingHintKey>` and
`HashMap<i32, RenderingHintValue>` populated lazily from
`RenderingHints.KEY_ANTIALIASING.hashCode()` etc. as observed. Even an
incomplete mapping (only the AA key and on/off values) would let the
hot disable-AA path work correctly.

---

## [MED-9] Multiple `Vec` allocations per draw call in `with_gfx`/array helpers
**File:** `native-awt/src/natives.rs:192-203` (`read_int_array`), call sites at 580–605

`drawPolygon` / `fillPolygon` / `drawPolyline` go through
`read_int_array` for both the `xs` and `ys` arrays — two fresh
`Vec<i32>::with_capacity(len)` plus `len` calls through
`ctx.get_array_element`. Then `Graphics2DState::draw_polygon` calls
`zip_points` (`graphics2d.rs:550-552`), which allocates a *third*
`Vec<(i32, i32)>` of the same length.

For high-rate stroke-by-polyline drawing (e.g. drag-to-draw freehand
input), this is three short-lived allocations per event. The
allocations also live inside the global `gfx_registry` mutex critical
section (see HIGH-3).

**Fix:** (a) Pass `xs`/`ys` as slices directly into a renderer entry
point that accepts disjoint x/y slices instead of zipped tuples
(trivial: edit `SoftwareRenderer::draw_polygon` to take `&[i32], &[i32]`).
(b) Cache a per-EDT thread-local scratch buffer for `read_int_array`'s
output — these are read on the EDT so a `RefCell<Vec<i32>>` is sound.

---

## [LOW-10] `clear_rect` hard-codes restore to `SrcOver` (latent bug)
**File:** `native-awt/src/graphics2d.rs:471-481`

`clear_rect` saves the colour, switches composite to `Src`, fills, then
sets composite to `SrcOver` *unconditionally*. Today this is invisible
because (a) there is no public `set_composite` API on `Graphics2DState`
and (b) `SavedState::composite` is hardcoded to `SrcOver` at
`graphics2d.rs:505`. When `setComposite(AlphaComposite)` is plumbed
through (it currently isn't, but is part of the Java2D API surface),
`clear_rect` will silently revert any user-set Xor/Clear composite back
to SrcOver.

Two cheap fixes: (a) save and restore `self.renderer.composite_mode` via
a getter the way `color` is handled, or (b) leave a `TODO` linked to a
future `set_composite` implementation. Recommend (a) — it's two lines and
removes the foot-gun before it lands.

---

## Summary

| # | Sev | File | Impact |
|---|-----|------|--------|
| 1 | HIGH | platform/x11.rs:716 | Reloads TTF from disk every text op |
| 2 | HIGH | platform/win32.rs:580 | Rebuilds DWrite factory + HFONT every text op |
| 3 | HIGH | natives.rs:97 | Global mutex on every Graphics2D op |
| 4 | HIGH | natives.rs:619 | Full image pixel copy per drawImage |
| 5 | HIGH | graphics2d.rs:573 | Per-pixel scalar gradient fill |
| 6 | HIGH | natives.rs:39 + edt.rs:106 | Unbounded growth of invocation side-tables |
| 7 | MED | natives.rs:1061 | FontMetrics always reads garbage font size |
| 8 | MED | natives.rs:760 | RenderingHints setter ignores key/value, forces AA on |
| 9 | MED | natives.rs:192 + graphics2d.rs:550 | 3× Vec alloc per polygon draw |
|10 | LOW | graphics2d.rs:471 | `clear_rect` hardcodes composite restore to SrcOver |
