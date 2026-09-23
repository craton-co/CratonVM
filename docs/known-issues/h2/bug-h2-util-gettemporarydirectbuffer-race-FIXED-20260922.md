# H2 — `sun.nio.ch.Util$BufferCache` race silently fell back to real bytecode under `--jdk-only` — FIXED 20260922

| | |
|---|---|
| **Status** | FIXED, 2026-09-22, dev branch |
| **Scope** | 11 of the 22 "genuinely new" classes from `nonpassed-classbyclass-census-20260922.md`, plus `TestFtp` from that same census's already-adjudicated wall-clock bucket |
| **Root cause** | `sun/nio/ch/Util.getTemporaryDirectBuffer`/`releaseTemporaryDirectBuffer`/`offerFirstTemporaryDirectBuffer`/`offerLastTemporaryDirectBuffer` were registered `NativeKind::Bridge`. Under `--jdk-only` (default since 2026-09-20), `resolve_native_dispatch_wave1`'s §7 step 3 sends any `Bridge` back to real bytecode whenever that bytecode exists — and it does, `sun/nio/ch/Util` is a real JDK class. So the native that exists specifically to avoid a real bug in that bytecode never ran. |

## The symptom

Eight classes crashed identically:

```
NullPointerException: Cannot invoke "java.nio.ByteBuffer.capacity()" because "buf" is null
```

(`TestReadOnly`, `TestCluster`, `TestUrlJavaObjectSerializer`, `TestAutoReconnect`,
`TestJakartaServlet`, `TestPageStoreCoverage`, `TestPgServer`, `TestServlet`) —
and three more with related buffer-corruption shapes traceable to the same
mechanism (`TestJavaObjectSerializer`/`TestAutoServer`:
`BufferUnderflowException`; `TestSpatial`: `IllegalArgumentException:
newPosition > limit`; `TestUpgrade`: a garbage-index
`ArrayIndexOutOfBoundsException`). All eleven PASS on stock HotSpot 25, same
host, same heap, same cap (measured — see `nonpassed-classbyclass-census-20260922.md`'s
hotspot control rerun, all 23 classes checked that day PASS on HotSpot in
under 30s each). `TestFtp`, listed in the census as a 300s HANG, also turned
out to be gated by the same mechanism — it PASSes in under a second once the
native runs.

## Why: the real bytecode has a documented, deliberately-worked-around bug

`native-io/src/direct_buffer.rs`'s `temporary_direct_buffer_get` /
`temporary_direct_buffer_release` exist because `sun.nio.ch.Util$BufferCache`
is a real JDK `ThreadLocal`, but CratonVM can expose the **same** cache
instance to several VM worker threads. Its unsynchronised
`count`/`start`/`ByteBuffer[]` ring bytecode then hands out a `count` that
claims a null array slot — the caller's very next `buf.capacity()` NPEs. The
comment above the native (dated well before this fix, written when the native
was added) describes this exact failure shape. The native's fix: keep the
pool **process-native** (a `thread_local!` Rust-side pool, one lock per
operation) instead of trusting the racy Java-side cache.

## Why the native stopped running: `--jdk-only`'s §7 step 3

Confirmed empirically by instrumenting three layers of the dispatch decision
(`temporary_direct_buffer_get` itself, `force_native_over_real_jdk_bytecode`,
and `resolve_native_dispatch_wave1`):

1. `force_native_over_real_jdk_bytecode(class, method, desc)` was **not**
   consulted for `sun/nio/ch/Util`'s four methods at all — they weren't in
   that ~55-branch allowlist. Adding them there (first attempt) made
   `compat_native_wins` become `true`, but the native **still never ran**.
2. The actual blocker is `vm/src/vm/vm_exec.rs::resolve_native_dispatch_wave1`,
   which every dispatch door consults once `compat_native_wins` is `true`:
   ```rust
   match kind {
       NativeKind::SyntheticStub => ...,          // §1.3 — refused under --jdk-only
       NativeKind::Intrinsic => Some(Intrinsic),   // §1.4 — the reviewed exception
       NativeKind::Bridge if bytecode_available => // §7 step 3
           { record_native_shadows_bytecode(..); None }  // <- real bytecode wins
       NativeKind::Bridge => Some(NativeBridge(callback)),
   }
   ```
   `getTemporaryDirectBuffer` et al. were registered `NativeKind::Bridge`
   (`native-io/src/direct_buffer.rs::register_direct_buffer_real`, wrapped in
   `r.set_category(NativeKind::Bridge)`). Under `--jdk-only`, **any** `Bridge`
   loses to real bytecode whenever that bytecode exists — being on the
   force-list is necessary but not sufficient. Only `NativeKind::Intrinsic`
   ("a correct fast-path... returns the same answer the real bytecode would...
   never gated") survives this rule.
3. These four methods were *also* present in
   `native-api/src/retired_shadow.rs`'s `RETIRED_SHADOW_L4_BUFFERS2_TRIPLES`
   (two of the four rows — `getTemporaryDirectBuffer` and
   `offerFirstTemporaryDirectBuffer`), a wave-17 (2026-09-19) bulk retirement
   of 65 rows whose own doc comment says the retirement rests on **single-shot
   lane-4 probes** (`invocations > 0`, zero strict diff against HotSpot on a
   debug binary) — exactly the kind of check that cannot see a
   **concurrency** race. That table's gate only re-tags a registration to
   `SyntheticStub` when the ambient category is `Bridge`, so it never applied
   to begin with once the kind changed to `Intrinsic`, but the stale entries
   were removed for honesty (see below).

## The fix

Two changes, both landed together:

1. **`native-io/src/direct_buffer.rs`** — re-tag the four
   `sun/nio/ch/Util` registrations `NativeKind::Intrinsic` instead of
   inheriting the file's ambient `NativeKind::Bridge` (a scoped
   `set_category`/`register`×4/`set_category` restore). This is the
   substantive fix: it is the only kind `resolve_native_dispatch_wave1`
   exempts from the "real bytecode wins when available" rule.
2. **`vm/src/runtime/interpreter/native_override.rs`** —
   `force_native_over_real_jdk_bytecode` also gained explicit entries for all
   four triples. Not load-bearing for the crash itself (verified — the
   `Intrinsic` retag alone is what makes the native run), but needed so
   `compat_native_wins` is `true` under `--compatible` too and so the JIT's
   own `force_native_over_real_jdk_bytecode`-consulting call sites (direct-call
   sealing in `jit_bridge.rs`, the interface-blind shadow probe) treat these
   four consistently with every other call site.
3. **`native-api/src/retired_shadow.rs`** — removed the two now-superseded
   rows from `RETIRED_SHADOW_L4_BUFFERS2_TRIPLES` (65 rows/20 classes → 63
   rows/19 classes) with a comment explaining why, and updated the
   `wave_seventeen_buffers_are_sixty_five_rows_over_twenty_classes` test
   (renamed `..._sixty_three_rows_over_nineteen_classes`) to match. Leaving
   stale "retired" entries for a native now registered `Intrinsic` would have
   been misleading even though the gate they feed no longer reaches them.

## Verification

- All four dispatch layers traced end-to-end on `TestReadOnly` before/after:
  confirmed the native fires (was 0 calls, now thousands per class) and the
  11-class cluster above flips FAIL/HANG → PASS.
- `cargo test -p cratonvm-native-api --lib`: 478/478 pass, including the
  renamed wave-17 test.
- Full 218-class H2 suite rerun (`jit-real`, `--jdk-only` default, `--max-heap
  1g`, 300s cap) for regressions — see the commit for the results file;
  nothing in the previously-passing set regressed.

## Related

- `nonpassed-classbyclass-census-20260922.md` — the census this closes 11 (of
  22 "genuinely new") rows from.
- `docs/known-issues/jdk-only/lane-4-handoff-20260919.md` /
  `lane-4-io-nio-foreign.md` — wave 17's own handoff, which flags "confirm
  each sweep really reads data back before relying on it" as still owed; this
  is the confirmation that two of its 65 rows do not hold under concurrency.
- Still open from the same census, **not** touched by this fix: a second,
  unrelated cluster (`TestAlterSchemaRename`, `TestTriggersConstraints`,
  `TestView`, `TestSampleApps`) failing with `NoSuchMethodError:
  'boolean java.nio.file.FileSystem.needToResolveAgainstDefaultDirectory()'`
  inside in-process `javac` (H2's `SourceCompiler`, used by `CREATE ALIAS ...
  $$ ... $$`). Traced as far as: the default-filesystem-as-real-`LinuxFileSystem`
  mint (`native-builtins/src/phases_late/nio_file.rs::p57_alloc_default_filesystem_unix`,
  fixed 2026-09-18) succeeds when called directly/in isolation, yet the
  `UnixPath` receiver reaching `getByteArrayForSysCalls` during javac's
  `Locations$SystemModulesLocationHandler.initSystemModules` walk is still the
  synthetic/abstract-stamped shape — so *some* earlier call in the same
  process must be getting (and, per real `FileSystems.getDefault()`
  semantics, permanently caching) the synthetic fallback before the real mint
  ever gets a chance. Not yet root-caused; `TestBtreeIndex` also remains an
  unexplained genuine HANG (HotSpot: 2.5s).
