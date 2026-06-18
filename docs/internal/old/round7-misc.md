# Round-7 Misc Review

Scope: `native-awt/`, `cuda-bridge/`, `jit-cuda/`, `jit-api/`, `types/`, `vm-cli/`.

## (A) Round-6 Wave-1 Audit

### native-awt/src/edt.rs::take_runnable — PASS
Lock ordering is globally consistent. Every site acquires `runnables`
BEFORE `runnables_order`: `register_runnable` (168-169), `take_runnable`
(195-196), `stop` (241-242). The remaining touchpoint
`notify_if_invocation` (389) takes only `runnables`. No deadlock risk.

### cuda-bridge/src/backend_cuda.rs — LOW (event accumulation)
`launch_raw` records a new event and queues a `copy_d2h.wait(&evt)` on
EVERY launch (246-252). Back-to-back kernel launches with no D→H
consumer accumulate dead waits in `copy_d2h`'s queue, forcing the next
`cuStreamSynchronize` to drain them. Cache one cudarc event per
`compute` stream and re-record only when a `to_host` is actually
enqueued.

### jit-cuda/src/analyzer.rs::scan_and_estimate — PASS
Single-pass walk (187-219) is bit-for-bit equivalent to the old
two-pass for **eligible** methods: classifier and branch-direction
probe run on the same `pc` cursor. For rejected methods the estimate
is dropped, matching previous behaviour. Pre-existing caveat: a
truncated jump at end-of-bytecode lets `pc` step past `bytes.len()`
(line 216).

## (B) Remaining Findings

### native-awt — HIGH: glyph atlas unwired
`font.rs:443 GlyphAtlas` is implemented but `rasterize_text` in
`platform/x11.rs:566`, `platform/cocoa.rs`, `platform/win32.rs` call
`font.rasterize(ch, font_size)` directly per char per call. Every
repaint re-rasterizes every glyph. Thread a `&GlyphAtlas` into
`Backend` and route through `atlas.get_or_rasterize`.

### native-awt — HIGH: gradient fill per-pixel
`graphics2d.rs:574 fill_rect_gradient` calls `set_color` +
`draw_pixel` PER PIXEL (580-588). Each pixel re-dispatches through
`put_pixel`. Precompute per-row gradient delta, build a row buffer,
reuse the existing `composite_row_src_over` path with a single
clip-test per row.

### native-awt — MED: `blit_image_scaled` has no identity fast path
`renderer.rs:1122` always falls back to `put_pixel` per output pixel
even when `transform.is_identity()` (1133-1151). Mirror the
`blit_image` fast-path: compute clipped dst rect once, walk rows with
direct slice writes and inline the sampler.

### types/src/value.rs — LOW: from_raw / from_raw_nonnull divergence
`from_raw` (line 77) unconditionally `panic!`s on unaligned ptrs
(line 86); `from_raw_nonnull` (102) uses `debug_assert!` (103).
Release-build alignment violations diverge between the two ctors.
Standardize on one — `debug_assert!` for both is consistent with the
"callers maintain alignment" doc.

### vm-cli/src/main.rs — MED: synchronous dir scan on every `--jar`
Lines 617-632 do `read_dir` against 4 hardcoded paths (`lib/lib/main`,
`lib/lib/boot`, `lib/quarkus`, `lib/app`) for EVERY jar startup —
non-Quarkus apps still pay the cost. Gate behind a sibling
`quarkus-application.dat` probe or an opt-in flag.

### cuda-bridge/src/backend_cuda.rs — MED: pinned host alloc missing
`from_host` (306) uses `memcpy_stod` on a pageable slice; the H→D DMA
stalls on page-locking. Add a `pinned_host_alloc` helper backed by
`cuMemHostAlloc(CU_MEMHOSTALLOC_PORTABLE)` and opt in for buffers
above ~64 KiB.

### cuda-bridge/src/lib.rs — LOW: hard-coded block size, no auto-tune
`LaunchConfig::elementwise` (line 67) hard-codes `block=256`. Query
`cuOccupancyMaxPotentialBlockSize` once per `(module, function)` at
load time and cache on `DeviceModuleInner::functions`.

### jit-api/src/lib.rs — LOW: helper-table drift hazard
`JitRuntimeHelpers` has 37 fields but `all_pointers()` /
`field_names()` hard-code `[usize; 32]` (lines 158, 195). New fields
silently drop out of `validate()` / `null_pointers()`. Replace the
two parallel arrays with a single
`const FIELDS: &[(&str, fn(&Self) -> usize)]` table.
