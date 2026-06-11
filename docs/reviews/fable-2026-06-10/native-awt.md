# native-awt crate review — 2026-06-10 (Fable / Opus)

Scope: `native-awt/src` (17 files, ~14.7k LOC). The crate backs `java.awt`,
`javax.swing`, and `java.awt.image` (Java2D) for CratonVM. Per `lib.rs` it runs
in **headless-only** mode: a software rasterizer renders into ARGB pixel
buffers; the three platform backends (`win32`, `cocoa`, `x11`) compile but are
**not wired through `natives.rs`** — `Frame.setVisible(true)` opens no window.

## Summary

Overall the crate is unusually defensive for a JVM native layer. Image
allocation, bulk `getRGB`, the SSE2 compositor, and the Win32/Cocoa FFI blit
paths are all guarded with `checked_mul`, signed-before-cast validation, and
RAII drop guards. The EDT has been through a documented race audit and uses
consistent lock ordering.

The material findings are: (1) two **unbounded-allocation / unbounded-loop**
paths reachable directly from Java draw calls (`fillArc`/`drawArc` and
`copyArea`); (2) the headless `drawString` renders **block-glyph placeholders**,
not real glyph shapes — the real fontdue rasterizer only lives in the unwired
platform backends; (3) synthetic font metrics; (4) several documented
unbounded-growth registries (peer, image) with no end-of-life hook; (5)
thin test coverage on `natives.rs` glue and **zero** tests on the `cocoa`/`x11`
backends despite `lib.rs` claiming "their own unit-test coverage".

No `unimplemented!`/`todo!`/`NotImplemented` macros that panic. No
command-injection or path-traversal surfaces (no shell-outs; clipboard is
in-process text-only; file dialogs are no-ops).

## Bugs

### B1 (high) — `fillArc`/`drawArc`: unbounded `steps` → OOM panic / hang
`renderer.rs:1040` (`fill_arc`) and `:986` (`draw_arc`):
```rust
let steps = ((rx.max(ry) as f32 * arc_angle.abs() / 90.0).ceil() as usize).max(16);
...
let mut points = Vec::with_capacity(steps + 2); // fill_arc, :1045
```
`rx`/`ry` are `(w/2) as u32` from the Java arc width/height
(`graphics2d.rs:392-404`), bounded only by a `w < 0` reject — so up to ~1.07e9.
`arc_angle` is the Java `extent` cast to `f32`. `Graphics.fillArc(0, 0,
Integer.MAX_VALUE, Integer.MAX_VALUE, 0, 360)` makes `steps ≈ 1e9 * 360/90 =
4e9`, so `Vec::with_capacity(~4e9)` requests ~32 GB → allocation abort.
`drawArc` doesn't pre-allocate but loops `steps` (~4e9) times → effective hang.
Also: a NaN/inf `arc_angle` makes `(...).ceil() as usize` saturate to
`usize::MAX` (Rust saturating float cast) → instant `with_capacity(usize::MAX)`
panic. Fix: clamp `rx`/`ry` to the buffer extent and cap `steps` to a sane
maximum before allocating.

### B2 (high) — `copy_area` fallback: `w * h` u32-overflow panic
`renderer.rs:1411`:
```rust
let mut temp = Vec::with_capacity((w * h) as usize);
```
`w`/`h` reach `copy_area` as `u32` straight from Java
(`natives.rs:1351`: `get_int(args, 3).max(0) as u32`, up to ~2.1e9). `w * h` is
a **u32 multiply** — it panics on overflow in debug and wraps in release (giving
a capacity smaller than the loop pushes, forcing reallocation). This branch
fires whenever the source rect is partly out of bounds, which Java fully
controls (`copyArea(x, y, w, h, dx, dy)` with `x<0` or `x+w>width`). Use
`(w as usize).checked_mul(h as usize)` and bail, mirroring the image paths.

### B3 (low) — `fill_polygon` scanline loop spans full vertex Y range
`renderer.rs:1105`: `for y in min_y..=max_y` where min/max are raw polygon
vertex Y values. A `fillPolygon` (or `fillArc`-generated polygon) with vertices
near `i32::MIN`/`i32::MAX` runs billions of scanline iterations. Pixel writes
are clipped by `put_pixel`, so it is not OOB — but it is an unbounded-work hang
from untrusted input. Clamp the scan range to `[0, height)` (∩ clip) first.

### B4 (low) — `draw_rect` stroke arithmetic can overflow i32
`renderer.rs:774-776`: `h - 2 * sw as u32` and `h - 2 * sw`. `sw =
(self.stroke_width as i32).max(1)` where `stroke_width: f32` is Java-controlled
(`Graphics2D.setStroke`). A huge stroke width makes `2 * sw` overflow i32
(panic in debug) before the guard `h as i32 > 2 * sw` is even meaningful. Low
severity (stroke widths are normally tiny) but it is reachable. Clamp/validate
`stroke_width` on the way in.

## Vulnerabilities

### V1 (medium) — `read_int_array` trusts `array_length` for `Vec::with_capacity`
`natives.rs:455-466`: `Vec::with_capacity(ctx.array_length(obj))`. If
`array_length` can return an absurd value for a malformed/hostile array object,
this is an attacker-influenced eager allocation. The subsequent loop reads each
element, so a genuinely huge array would be `Vec`-bounded anyway, but the
`with_capacity` pre-reserve trusts the reported length without a cap. Bound the
reserve (e.g. `with_capacity(len.min(SANE_CAP))`) so a bogus length can't force
a giant up-front allocation.

### V2 (low, latent) — `bilinear_sample`/`bicubic_sample` assume `src.len() ≥ w*h`
`renderer.rs:1562-1565` and `:1638` index `src[(y*w + x)]` with x/y clamped only
to `w-1`/`h-1`. They are sound for the *current* caller (`blit_image_scaled`
fed by `BufferedImageData` whose `pixels.len() == w*h` invariant holds via
`try_new`), but the functions are free-standing with no internal length guard.
Any future caller passing a short slice panics OOB. Add a `src.len()` guard or
document the precondition as `# Safety`-style.

### V3 (low, by-design) — `lookup_peer_source` reconstructs `ObjectRef::from_raw`
`natives.rs:239`. A raw heap pointer is cached and later rebuilt into an
`ObjectRef`. The GC-generation gate (`entry.gc_gen != current_gc_gen → None`,
`:228`) fails closed on *any* collection, which is the right mitigation, and the
SAFETY comment is accurate **iff** `gc_collection_count()` increments on every
collection (moving or not). Flagged as a residual soundness assumption, not an
active defect — the same pattern is used elsewhere in the VM.

## Stubs and Unimplemented

- **`graphics2d.rs:437` `draw_string` renders block glyphs, not text.** The
  headless path draws each char as a filled/outlined rectangle (uppercase/digit
  → outline rect, lowercase → half-height fill). Real glyph rasterization
  (fontdue / DirectWrite) lives only in the **unwired** platform backends and in
  the `GlyphAtlas` (`font.rs`), which `natives.rs` never calls. Any app that
  renders text into a `BufferedImage` and exports/inspects it (charts, labels,
  captchas, PDF/PNG generation) gets placeholder boxes. This is the most
  material fidelity stub in the crate.
- **`font.rs:293` synthetic font metrics.** `compute_metrics` derives
  ascent/descent/advance from `size` alone (`0.8*s`, `0.2*s`, etc.) — not from
  real font tables. `FontMetrics.stringWidth`/`getHeight` will not match the
  real JDK, breaking layout-sensitive code.
- **Platform backends not instantiated** (`lib.rs:48-54`, confirmed: no
  `Win32Backend::new`/`X11Backend`/`CocoaBackend` references in `natives.rs`).
  `setVisible(true)` opens no window; mouse/key/window events from the backends
  are never plumbed to Java listeners.
- **Component / Focus / Action AWT events dropped.** `plan_event_synthesis`
  returns `None` for these (`natives.rs:1719-1721`); `getNextEvent` yields null
  and the dispatch cycle drops them (`natives.rs:1716`, `:1850`).
- **Clipboard is in-process text-only.** Non-text flavors (image / file-list /
  raw) yield `null` (`natives.rs:2188-2190`); there is no system-clipboard
  bridge in headless mode.
- **File dialogs are no-ops** in every backend: `win32.rs:1141` (`warn!("…not
  yet implemented")`), `cocoa.rs:711`, and X11 selection protocol
  `x11.rs:723` (`"X11 selection protocol not yet implemented"`).
- **`BufferedImage.initIDs` (and 12 sibling raster classes) registered as
  no-ops** (`natives.rs:1583-1599`) — legitimate (CratonVM resolves fields by
  name) but worth noting they intentionally do nothing.

## Performance

- **P1 — `GlyphAtlas` LRU eviction is O(n) per at-cap insert.** `font.rs:574-583`
  does `cache.map.iter().min_by_key(...)` over the whole map on every insert once
  full. On a churny glyph workload this is a linear scan on the text hot path.
  Use a proper LRU (intrusive list / `lru` crate) or a generational/2-hand clock.
- **P2 — `fill_polygon` reallocates intersection set per call.** `renderer.rs:1103`
  allocates a fresh `Vec` per `fill_polygon` (it is reused across scanlines via
  `clear()`, which is good, but not across calls). For polygon-heavy renders a
  pooled scratch buffer on the renderer avoids the per-call alloc.
- **P3 — `take_runnable_entry` linear scan of the order deque.** `edt.rs:276`
  does `order.iter().position(...)` on every dispatch. Bounded by
  `MAX_RUNNABLES` (10k) and usually hits the head, but worst case is O(n) per
  dispatch; a `HashMap<id, index>` or a tombstone scheme would make it O(1).
- **P4 — `fill_polygon`/`fill_arc` unbounded scan range.** See B3: the scanline
  loop runs `max_y - min_y` iterations regardless of buffer size. Clamping to
  the visible region is both a correctness (DoS) and a perf win.
- **P5 — Many natives take the global `gfx_registry()`/`image_registry()` mutex
  per call.** Every `drawLine`/`fillRect`/`setColor` locks a process-global
  mutex (`natives.rs` `with_gfx`, `gfx_handle_for`). For a tight draw loop this
  is lock-acquire-per-primitive contention; a per-context handle cached on the
  Java object (or a thread-local fast path) would help. Acceptable for headless.

## Tests

Estimated coverage: **~62%**.

Basis (read, not run): 236 `#[test]` functions. Strong, behavior-level coverage
on the pure-logic modules — `renderer.rs` (35: lines, rects, ellipses, blits,
affine, overflow-clamp at `:2035`), `graphics2d.rs` (30), `font.rs` (26),
`color.rs` (25), `image.rs` (21: incl. overflow-reject and OOB-panic tests),
`swing.rs` (21), `edt.rs` (22: race/lifecycle/GC-gate), `peer.rs` (15),
`event.rs` (12), `clipboard.rs` (11).

Gaps that keep it well under 85%:
- **`natives.rs` (~2519 LOC) has only 12 tests**, all on event synthesis / gfx
  reaping. The huge registration surface (graphics/image/font/swing natives) is
  largely exercised only indirectly. None of the input-validation branches
  flagged here (B1/B2, `getRGB` bulk bounds, `BufferedImage.<init>` rejects)
  have a direct unit test driving them through a `NativeContext`.
- **`platform/cocoa.rs` and `platform/x11.rs` have ZERO tests** despite
  `lib.rs:48` claiming "their own unit-test coverage". Only `win32.rs` has any
  (6, mostly the `next_window_id` counter). The FFI blit/raster guards are
  untested.
- No test reproduces B1 (`fillArc` huge-dim), B2 (`copyArea` overflow), B3/B4
  (extreme polygon/stroke). These are exactly the untrusted-input edges.
- No test for the `bilinear/bicubic` short-slice precondition (V2).

Does it plausibly reach 85%? **No** — the natives glue and two of three
platform backends are effectively untested.

Most important missing tests:
1. `fillArc`/`drawArc`/`copyArea` with `Integer.MAX_VALUE` dims (drives B1/B2).
2. `natives.rs` graphics/image natives through a mock `NativeContext` (bounds,
   overflow rejects, dispose/flush lifecycle).
3. `bilinear_sample`/`bicubic_sample` with a deliberately short `src`.
4. Any coverage for `cocoa`/`x11` `blit_buffer` guard logic (can be host-gated).

## Feature Suggestions

1. **Wire real glyph rendering into the headless path.** Route `draw_string`
   through the existing `GlyphAtlas` + fontdue (already built, currently dead in
   headless) so `BufferedImage` text is real glyphs, not boxes. This is the
   single biggest fidelity gain and unblocks chart/label/export apps.
2. **Real font metrics from an embedded font.** Replace `compute_metrics`'
   size-ratio approximation with fontdue line metrics for a bundled default
   face, so `FontMetrics.stringWidth` matches layout expectations.
3. **Bound all draw primitives to the surface.** A shared "clamp rect to buffer ∩
   clip, derive bounded step/scan counts" helper applied to arc/oval/polygon
   fixes B1/B3/P4 in one place and matches the discipline already in
   `fill_rect_raw`/`blit_image`.
4. **End-of-life reclamation for the peer and image registries.** Both are
   documented as unbounded-growth (`peer.rs:153`, `image.rs:310`). A
   Cleaner/PhantomReference hook or an idle-LRU would stop the leak when Java
   `dispose()`/finalizers are missed.
5. **Plumb Component/Focus/Action events** through `plan_event_synthesis` so
   Swing focus traversal and `ActionListener`-driven flows work headless.
6. **Wire one platform backend (start with win32) end-to-end** behind a feature
   flag so `setVisible(true)` can open a real window — the FFI is already
   written and guarded; only the natives→backend bridge is missing.

## Files sampled vs fully read

Fully read:
- `lib.rs`, `image.rs` (incl. all tests), `font.rs` (metrics + glyph atlas),
  `natives.rs` helpers + image/graphics/event/clipboard registration + event
  synthesis + peer-source side-table.
- `renderer.rs`: SSE2 compositors, `safe_buffer_dims`/`new`/`resize`,
  `put_pixel`/`fill_rect_raw`, `draw_arc`/`fill_arc`, `fill_polygon`,
  `blit_image`/`blit_image_scaled`/`copy_area`, `bilinear`/`bicubic`.
- `edt.rs`: registry/race-fix region, `invoke_and_wait`/`stop`/
  `signal_invocation_complete`/`take_runnable_checked`.
- `graphics2d.rs`: `draw_string`, draw_* primitives, stroke, image ops.
- `platform/win32.rs`: `blit_buffer`, `rasterize_text` (FFI/DIB guards).
- `platform/cocoa.rs`: `blit_buffer` guard (sampled, compared to win32).

Sampled / structure-scanned (grep + targeted reads, not line-by-line):
- `clipboard.rs`, `color.rs`, `event.rs`, `peer.rs` (registry section read),
  `swing.rs`, `platform/x11.rs`, `platform/backend.rs`, `platform/mod.rs`, and
  the bulk of `platform/win32.rs` outside the two FFI functions above.
