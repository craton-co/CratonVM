# Round-8 Misc Review

Scope: `native-awt/`, `cuda-bridge/`, `jit-cuda/`, `jit-api/`, `types/`, `vm-cli/`.

## (A) Round-7 Wave 1+2 Audit

### CRIT — cuda-bridge: `launch_raw_no_d2h_sync` is dead code
`cuda-bridge/src/backend_cuda.rs:171` defines the no-sync variant with
`#[allow(dead_code)]`, but no caller exists — not in
`lib.rs::DeviceModule::launch_raw` (line 133, which only routes to the
syncing variant), not in `jit-cuda`, not anywhere. The whole "Fix 3"
delivers zero benefit; every kernel still records a wasted event +
`copy_d2h.wait`. **Fix:** expose `DeviceModule::launch_raw_no_d2h_sync`
publicly in `cuda-bridge/src/lib.rs` and wire the `jit-cuda` ahead-of-
results launch path through it; or remove the function and rename the
flag to honest dead state.

### CRIT — native-awt: `GlyphAtlas::get_or_rasterize` double-rasterizes
`native-awt/src/font.rs:490-530` admits the race ("we may rasterize
twice; both bitmaps will be byte-equal and the second insert overwrites
the first"). That is wrong: both threads return their own
`Arc<GlyphBitmap>`, so `Arc::ptr_eq` differs for the same glyph and
downstream identity caches (e.g. atlas-page binding) break. The
overwrite also drops the *first* `Arc` from the map while the first
caller still holds it — every later `get` returns the *second* Arc, so
the cap-based eviction undercounts true live bitmaps. **Fix:** use
`entry(key).or_insert_with(|| rasterize())` under the lock; the
rasterize cost (~30 µs) is far less than the contention cost of a
duplicated bitmap.

### CRIT — vm-cli: Quarkus filename heuristic is case-sensitive on Windows
`vm-cli/src/main.rs:645-648` uses `n.contains("quarkus-") ||
n.ends_with("-runner.jar")` literally. On Windows/macOS where filesystems
are case-insensitive, `Quarkus-App-1.0-Runner.jar` (the Maven default
classifier output) misses both checks, the dir scan never runs, and
`ClassLoader.loadClass` fails at first lookup. **Fix:** lowercase once
(`let lower = n.to_ascii_lowercase()`) and match on
`lower.contains("quarkus-") || lower.ends_with("-runner.jar")`.

### HIGH — jit-api: `NUM_FIELDS` assertion is tautological
`jit-api/src/lib.rs:224, 264` claims `debug_assert_eq!(arr.len(),
Self::NUM_FIELDS)` will trip if a new struct field is added without
bumping the constant. It can't: `arr`'s compile-time type is
`[usize; Self::NUM_FIELDS]`, so `arr.len() == NUM_FIELDS` holds
identically. Adding a new struct field but forgetting to extend the
array literal silently passes — the new field is just absent from
validation. **Fix:** add a compile-time check
`const _: () = assert!(std::mem::size_of::<JitRuntimeHelpers>() ==
NUM_FIELDS * std::mem::size_of::<usize>() + EXTRA_OFFSET_FIELDS * ...);`
or list field accessors via a macro that enforces parity.

## (B) Round-7 Carryovers

### HIGH — jit-cuda: no `cuOccupancyMaxPotentialBlockSize` autotune
`jit-cuda/src/lowering.rs` and emit paths use a fixed block size; CUDA
exposes `cuOccupancyMaxPotentialBlockSize` to pick a block size that
maximises occupancy per kernel's register/shared usage. **Fix:** after
`module.load`, query each function's occupancy hint once and cache it
on `PtxKernel` so `LaunchConfig::elementwise` uses the real optimum
(commonly 128–256 vs the current hardcoded value).

### MED — native-awt: `drain_events` and `coalesce_paint_events` are
separate lock acquisitions
`native-awt/src/edt.rs:439, 458` each take `self.queue.lock()` independently.
The EDT poll loop typically calls coalesce → drain, taking the lock
twice per dispatch. **Fix:** add `drain_and_coalesce(&self)` that does
both under a single `lock()`, returning the coalesced `Vec` in one shot.

## (C) New Angles

### MED — cuda-bridge: no `device_count()` for multi-GPU enumeration
`cuda-bridge/src/lib.rs:110` accepts `device_ordinal` but exposes no
way for the VM to discover how many GPUs are present. Callers must
guess. **Fix:** add `pub fn device_count() -> Result<u32>` wrapping
`cudarc::driver::result::device::get_count`; document the
zero-based ordinal range.

### LOW — types: missing `#[cold]` on rare `CompactValue` arms
`types/src/value.rs` decode/encode paths don't mark the
`Value::Object(None)` and exception-carrying arms `#[cold]`. The branch
predictor learns it, but explicit hints let the optimiser keep the
hot arm fall-through.
