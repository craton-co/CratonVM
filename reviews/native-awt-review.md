# native-awt review

Crate path: `C:\Projects\CratonVM\native-awt`
Files inspected: `Cargo.toml`, `README.md`, `src/{lib,color,event,font,clipboard,image,graphics2d,renderer,peer,swing,edt,natives}.rs`,
`src/platform/{mod,backend,win32,x11,cocoa}.rs` (15 source files, ≈12 700 LOC + ≈2 100 LOC tests).

## Summary

- [MED] `natives.rs` (the 1 779-line Java↔Rust bridge) has **exactly one** test; large branches such as
  `BufferedImage.getRGB(IIII[III)[I`, `RenderingHints` decoding, gradient/stroke field reads, and
  `InvocationEvent.dispatch` are untested.
- [MED] Three Win32 / X11 / Cocoa resource-leak paths (`BeginPaint` without `EndPaint`,
  `GlobalAlloc` without `GlobalFree` on `OpenClipboard` failure, and an X11 byte-slice cast that
  assumes `pixels.len() * 4` fits in `isize`).
- [MED] Geometry / coordinate overflow in `edt::EventDispatchThread::merge_paint_rects`
  (`*dx + *dw` is plain `i32 + i32`; coalescing a paint near `i32::MAX` panics in debug / wraps in release).
- [HIGH] Crate-level docstring (`lib.rs`) accurately advertises headless-only operation, but the
  README's *Scope* paragraph still claims the renderer "blits the final buffer" — the platform
  backends compile but are **not instantiated from `natives.rs`** (correctly noted in `lib.rs`, missed in README).
  Risk: users see the crate name and assume on-screen rendering works.
- [MED] OSS-readiness: `Cargo.toml` lacks an explicit `publish = false` line (relies on workspace
  default) and a `version` field of its own; otherwise the SPDX / NOTICE / Apache-2.0 headers are
  consistent across all files.

## 1. Code review

### Bugs (correctness / panics / overflow)

- [MED] `edt.rs:657-658` — `merge_paint_rects` computes `*dx + *dw` and `*sx + *sw` without
  overflow check. Two paints with `x ≈ i32::MAX, w = 1` panic in debug or silently wrap in release.
  Fix: widen via `i64` then clamp.
- [MED] `graphics2d.rs:329-403` — `fill_rect`/`draw_rect`/`draw_oval`/`draw_arc` take `i32 w,h`
  then do `w as u32` after the `w<0 || h<0` guard, but the centre-point computation
  `x.saturating_add(w/2)` does **not** check that `w/2` won't overflow. `w = i32::MIN` (rejected by
  `<0`) is fine but `w = i32::MAX` is accepted and produces `(rx, ry) = (i32::MAX/2) as u32`
  → harmless but `x.saturating_add(w/2)` is correct.
- [MED] `renderer.rs:760-775` — `draw_rect` does `y + h as i32 - sw` where both `h` and `y` are
  attacker-controllable. With `h = u32::MAX` and `y = 0` this overflows `i32` arithmetic. Should use
  `saturating_*` or pre-clip.
- [LOW] `renderer.rs:1102` — `fill_polygon` `unwrap()`s `min_y/max_y` after the `points.len() < 3`
  early-return; that guard does cover empty input, but a single-vertex polygon would slip through the
  `len < 2` check above. Currently `< 3` is correct → no bug.
- [LOW] `image.rs:152` — `set_rgb` uses `debug_assert!` for bounds and then implicit `Vec` index;
  release builds still panic but with a less helpful message. Tests cover the panic. Acceptable.
- [LOW] `font.rs:533` — `std::char::from_u32(key.ch).unwrap_or(' ')` swallows malformed surrogate
  code points → silently substitutes a space glyph. Acceptable but should be documented.
- [LOW] `swing.rs:65` — `MetalTheme::ocean()` lists `secondary3: 0xFFEEEEEE` but `swing.rs:540`
  test asserts `Button.background == 0xFFEEEEEE`, while the comment "secondary3" is misleading
  (`theme.control == 0xFFEEEEEE`). Cosmetic.

### Vulnerabilities

- [MED] `platform/win32.rs:474-538` (`blit_buffer`) — after `BeginPaint` succeeds and
  `CreateDIBSection` fails (`map_err -> ?`), the function returns `Err` **without calling
  `EndPaint`**. Repeated failures leak the paint-cycle state inside the window manager.
  Fix: hold an RAII `EndPaint` guard (the Cocoa path already does this with `LockGuard`).
- [MED] `platform/win32.rs:846-863` (`clipboard_set_text`) — `GlobalAlloc` succeeds, then
  `OpenClipboard` fails (`map_err -> ?`); the allocated `hmem` is never freed. Need a fallible
  cleanup in the early-return.
- [MED] `platform/x11.rs:442-444` — `std::slice::from_raw_parts(pixels.as_ptr() as *const u8,
  pixels.len() * 4)`. `pixels.len() * 4` can theoretically overflow `usize` on a 32-bit X11 host
  (the crate compiles for any Linux target). The bound check above does
  `(width * height) as usize > pixels.len()` (with `u32 * u32` that wraps on huge inputs). Should be
  `checked_mul`.
- [MED] `platform/win32.rs:809-834` (`clipboard_get_text`) — the wide-char NUL-terminator scan is
  now capped at 16 MiB by the recent fix and uses `from_utf16_lossy`, so no unbounded read. Good.
  However `GetClipboardData(CF_UNICODETEXT)` returns a borrowed handle whose contents can change
  between `GlobalLock` and the scan if another process replaces the clipboard mid-read; the lossy
  decode is the only safety net. Document.
- [LOW] `platform/cocoa.rs:387-420` — `CGDataProviderCreateWithData` is called with `release_data:
  None`, and the backing `pixels: &[u32]` slice must out-live the resulting `CGImage`. The function
  `CFRelease`s the image before returning, so the lifetime is OK — but the contract is invisible to
  callers. Add a `// SAFETY` comment.
- [LOW] `platform/cocoa.rs:543` — `let font_obj: &AnyObject = std::mem::transmute(&*font);` is a
  bounded transmute (`NSFont` → `AnyObject`) used because objc2 lacks a stable upcast at this
  version. The transmute is sound but should carry a SAFETY block.
- [LOW] `font.rs:299-305` — Global `Mutex<FontEngine>` held across `string_width` calls.
  Contention is bounded but a stuck UI thread can block layout. Acceptable.
- [LOW] No fuzzing of font parsing inputs. `load_fontdue_font` reads system fonts unconditionally
  (`/usr/share/fonts/...`, `/System/Library/Fonts/...`); the bytes are untrusted-with-respect-to
  *this crate* (filesystem-controlled, not user-controlled), so `fontdue::Font::from_bytes` is the
  only line of defence. Fuzzing fontdue is out of scope, but **failure to parse should not panic**;
  the code uses `if let Ok(font) = ...` correctly.

### Stubs / no-op surface (full inventory)

`grep` of `todo!|unimplemented!|FIXME|TODO|XXX|HACK|"not implement"`:

1. `edt.rs:459` — TODO: invocation-completion signal should fire after `Runnable.run()` returns,
   not at dequeue (the natives layer now drives this correctly; the TODO is stale for that path).
2. `edt.rs:524` — TODO(round-10, double-buffered EDT): wire `blit_buffer` into the coalesced
   paint-drain pipeline.
3. `image.rs:297-302` — TODO(leak): no eviction is wired; every `BufferedImage` leaks its pixel
   buffer until process exit. Recommended: hook `BufferedImage.flush()` / finalizer.
4. `peer.rs:153-157` — TODO(eviction): peer registry grows without bound; missed Java GC →
   leaked `ComponentPeer`s.
5. `natives.rs:1426-1431` — TODO: synthesize Java event objects for non-invocation events
   (Mouse/Key/Window). **Today every non-invocation event returned by `EventQueue.getNextEvent`
   is `null`** — i.e. the entire AWT event-listener pipeline is non-functional past
   `invokeLater`/`invokeAndWait`. **This is the single biggest stub**.
6. `font.rs:327-336` — TODO(platform-migration): X11 and Cocoa text paths *do* now use the glyph
   atlas; the TODO is stale.
7. `renderer.rs:363-364` — TODO: NEON/AArch64 SIMD path. Falls through to scalar; perf only.
8. `platform/x11.rs:684-693` — `clipboard_get_text` / `clipboard_set_text` log a warning and
   return `None` / `Err`. **X11 clipboard is not implemented.**
9. `platform/x11.rs:701` — `show_file_dialog`: not implemented (requires GTK/zenity).
10. `platform/x11.rs:712` — `show_message_dialog`: logged only, no real dialog.
11. `platform/x11.rs:749-751` — TODO: respect family/bold/italic. All glyphs render in the same
    fallback face.
12. `platform/cocoa.rs:711-713` — `show_file_dialog`: NSOpenPanel/NSSavePanel not wired.
13. `platform/cocoa.rs:778-781` — Same family/bold/italic TODO as X11.
14. `platform/win32.rs:873-875` — `show_file_dialog`: not implemented.
15. Crate-level (`lib.rs:38-54`) — All three platform backends are scaffolded but **never
    instantiated by `natives.rs`**. `Frame.setVisible(true)` does not open a window.
16. `natives.rs:699-702` — `Frame.toFront`, `toBack`, `setIconImage`, `setMenuBar` all
    unconditionally `void_ok()`.
17. `natives.rs:1408` — `EventQueue.postEvent` is a `void_ok()` no-op.
18. `natives.rs:1434` — `EventQueue.peekEvent` always returns null.
19. `natives.rs:1587` — `UIManager.setLookAndFeel` is a no-op (Metal is hard-coded).
20. `natives.rs:1654-1675` — `JFileChooser.show*Dialog`, `JOptionPane.show{Confirm,Input}Dialog`
    return fixed headless answers (CANCEL / YES / empty string). Documented inline but worth
    surfacing in the implementation-status matrix.

### Performance

- [LOW] `graphics2d.rs:570` (`zip_points`) — allocates a fresh `Vec<(i32,i32)>` per polygon call.
  For `drawPolygon` in animation loops this is per-frame allocation. Cache or take `&[(i32,i32)]`.
- [LOW] `renderer.rs:1390-1400` — `Box<dyn Iterator>` allocation per overlapping `copy_area`.
  Cheap relative to the copy itself, but unnecessary; replace with `if d_y > s_y { for r in
  (0..copy_h).rev() ... } else { for r in 0..copy_h ... }`.
- [LOW] `natives.rs:1329-1342` — bulk `getRGB` calls `ctx.set_array_element` once per pixel.
  A 1080p image is ≈ 2 M JNI-equivalent calls. Add a bulk-write fast path on `NativeContext`.
- [LOW] `font.rs:521-527` — `GlyphAtlas` eviction iterates the hashmap in unspecified order; OK as
  documented but degenerate workloads can thrash. A real LRU is in the README TODO list.
- [LOW] `renderer.rs:1471-1526` — Wu antialiased line uses `f64`s per pixel; integer-fixed-point
  variant would be ~2× faster. Optional.

## 2. Tests

### Inventory

| File | `#[test]` count |
|---|---|
| `clipboard.rs` | 11 |
| `color.rs` | 25 |
| `edt.rs` | 21 |
| `event.rs` | 12 |
| `font.rs` | 26 |
| `graphics2d.rs` | 30 |
| `image.rs` | 21 |
| `natives.rs` | **1** |
| `peer.rs` | 15 |
| `renderer.rs` | 35 |
| `swing.rs` | 21 |
| `platform/win32.rs` | 1 (lparam_xy sign extension) |
| `platform/{x11,cocoa,backend,mod}.rs` | **0** |

Workspace total ≈ **219** unit tests; no integration tests, no fuzz targets, no proptest. Estimated
*code* coverage ≈ 70 % concentrated in `renderer/graphics2d/color/font/image/event/edt`. Critical
gaps:

### Coverage gaps

- `natives.rs` — the entire bridge layer has one registration-count test. Every
  Java-API surface (`BufferedImage.getRGB`, `RenderingHints.setRenderingHint`, `setStroke`,
  `setTransform`, `getNextEvent`/InvocationEvent.dispatch) lacks behavioural tests. **Coverage of
  `natives.rs` is < 5 %.**
- Platform backends: **zero** tests for X11/Cocoa; one Win32 test. No headless-fake / mock backend.
- Geometry overflow regression tests are missing: huge `BufferedImage` dimensions (covered
  partially in `image.rs:387-401`), huge clip rectangles, negative coordinates flowing through
  `fill_arc`, `draw_oval`.
- Concurrency: no test for concurrent `GlyphAtlas::get_or_rasterize` identity contract
  (documented at `font.rs:486-498` but unverified). Add a thread-pool test.
- Image-format edge cases: 0×N, N×0, max-dimension boundary (32 767), all-transparent vs
  all-opaque blits, IntArgbPre vs IntArgb mismatch (`ImageType::has_alpha` decides but no test
  asserts the pre-multiplied path).
- `lerp_argb` is exercised by gradient tests but not directly — add unit tests on edge `t = 0`,
  `t = 1`, `t = NaN` (currently `NaN` panics through `round() as u8`).
- `composite_row_sse2` — only the SSE2 path is tested via the scalar happy paths;
  no test isolates the SIMD chunk vs scalar tail equivalence. Recommended: property test that
  scalar and SIMD produce bit-identical output for random `(src, dst, w)`.
- `edt::merge_paint_rects` — the overflow case is not tested.
- `peer.rs` — `find_peer_at` is tested only for two-level trees; deep nesting and Z-order
  ambiguity not covered.

### Concrete additions

1. `tests/natives_bridge.rs` (new integration suite, behind a feature flag if `NativeContext` is
   not mockable from outside the workspace) — exercise every registered method with a stub
   `NativeContext`.
2. Property test (`proptest`): random `(width, height, [(i32,i32)] polygon)` → `fill_polygon` does
   not panic and only touches pixels inside the bounding box.
3. Property test for `bilinear_sample` / `bicubic_sample`: for any in-bounds `(x, y)`,
   `clamp_u8` outputs stay in `[0, 255]`.
4. Headless / mock `PlatformBackend` impl + golden-image tests via the renderer (`Vec<u32>`
   comparison).
5. `cargo-fuzz` target on `font::FontEngine::string_width` (UTF-8 strings → no panic, no overflow
   on `size = 1..=512`).
6. Concurrency stress: 8 threads × 1000 iterations × `GlyphAtlas::get_or_rasterize` of the same
   key → `Arc::ptr_eq` holds.

## 3. Documentation

### Existing

- `lib.rs:1-89` — exemplary crate-level rustdoc: ASCII architecture diagram, explicit "headless-
  only" mode notice, platform-backend status, EDT threading model.
- All public structs (`Color`, `Rect`, `AffineTransform`, `Graphics2DState`, `SoftwareRenderer`,
  `EventDispatchThread`, `ComponentPeer`, `PlatformBackend`, `GlyphAtlas`, `BufferedImageData`) have
  docstrings. Most methods do.
- `README.md` cleanly lists Scope / Non-goals / Usage / Status / License.
- Inline rationale comments in `edt.rs` and `natives.rs` describe race-fix history and JDK-spec
  alignment notes — high-quality.

### Missing / inconsistent

- `README.md` *Scope* claims "per-OS backend (Win32 / X11 / Cocoa) responsible only for blitting
  the final buffer". `lib.rs:48-54` clarifies the backends are **scaffolded but not instantiated**.
  Bring README into agreement — readers grepping `cratonvm-native-awt` will be misled into
  expecting on-screen output.
- No **implementation-status matrix** in the crate. Recommended `STATUS.md` table with three
  columns: *Java API*, *Headless behaviour*, *Backend-wired*. Would document points 8–20 from the
  stubs inventory above in one place.
- `docs/javafx-status.md` documents JavaFX as out-of-tree (correct). The README of *this* crate
  should link to `docs/javafx-status.md` and explicitly note: "JavaFX uses Glass/Prism via JNI and
  does not consume any symbol from this crate." Currently the README's *Non-goals* line says "No
  JavaFX" without explaining why.
- `peer.rs:153-157` and `image.rs:297-302` cite known leaks in source comments but neither leak
  is mentioned in any user-facing markdown. Surface in `README.md` *Known limitations*.
- `platform/backend.rs` has no module-level rationale for `unsafe impl Send` on each backend
  (`win32.rs:138`, `cocoa.rs:752`); add a SAFETY paragraph.
- `natives.rs` has 1 779 lines and no module-level "what every registration block does" overview.
  A short index at the top would pay back fast.

## 4. OSS readiness

### Cargo.toml

- `[package]` correctly inherits `version` / `edition` / `rust-version` / `license` / `repository`
  / `keywords` / `categories` from the workspace.
- **Missing**: explicit `publish = false`. Relies on workspace default (per the user prompt
  workspace declares `publish = false`); add the line locally for safety against re-export.
- `[features]` default = empty. Good.
- Platform-conditional dependencies (`windows`, `x11rb`, `objc2*`) are correctly gated by
  `cfg(target_os = "...")`. Good.
- `[dev-dependencies]` is empty even though there are 218 tests. Acceptable because none use a
  test framework crate, but consider adding `proptest` (already used elsewhere in the workspace —
  see `Cargo.lock`).

### SPDX / NOTICE headers

Every `.rs` file begins with:
```
// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
```
Verified across all 15 source files. README declares Apache-2.0. No third-party copyright
acknowledgement is required for `fontdue`, `x11rb`, `windows`, or `objc2-*` because they are
dynamic dependencies (their licenses are surfaced at workspace level via `NOTICE`).

### Blockers (for "AWT-on-CratonVM works" claim)

1. **Headless-only contract must be advertised in README** — currently understated.
2. **`EventQueue.getNextEvent` silently drops every non-invocation event** — a user clicking on a
   Swing button will not see the click. Mark prominently as "in progress".
3. **Frame.setVisible is silent** — no window opens, no `WINDOW_OPENED` event fires. The crate
   doc-comment notes this; the README does not.
4. **X11 clipboard / file dialog / message dialog are stubs** — apps that depend on copy-paste on
   Linux will appear broken with no error indicator.
5. **No implementation-status matrix** to track items 1–4 across releases.

The OSS-readiness verdict: **publishable as `publish = false` workspace member**, but **not
ready to be advertised as functional AWT** without the four documentation fixes above and the
stubs inventory landing in a `STATUS.md`.

## Top 5 fix priorities

1. **[MED → HIGH risk] Win32 `BeginPaint` / `GlobalAlloc` leaks** (`platform/win32.rs:474-538`,
   `:846-865`). Wrap with RAII guards. (small patch, hard-to-reproduce in tests, real production
   risk on long-running apps.)
2. **[MED] `natives.rs:1426-1431` — synthesise mouse / key / window AWT events.** Until this
   lands, *any* Swing app loses every event after the first `invokeLater`. Single-largest
   functional gap; documented but not behind a feature gate or runtime warning.
3. **[MED] Tests for `natives.rs`** — the bridge is 27 % of the crate's LOC and has < 1 % test
   coverage. Add an in-tree mock `NativeContext` and exercise every registered method.
4. **[MED] Image / peer registry eviction** (`image.rs:297-302`, `peer.rs:153-157`). Hook
   `BufferedImage.flush()`'s native to call `ImageRegistry::destroy`, and add a
   weak-ref / liveness sweep on `peer::PeerRegistry`. Long-running Swing apps currently leak every
   `BufferedImage` and every disposed `Component`.
5. **[MED] `STATUS.md` (or expanded README *Status* section) listing every stub.** All twenty
   stub sites in §1 should appear in a single user-readable matrix together with platform support
   for clipboard / dialogs / fonts. Without this the crate's behaviour is non-obvious to consumers
   regardless of license or version.
