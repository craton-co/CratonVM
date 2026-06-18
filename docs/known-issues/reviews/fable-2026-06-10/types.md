# CratonVM `types` crate — code & test review (2026-06-10)

Scope: `types/src` (8 files, ~5.8k LOC incl. tests) + `types/tests` (2 integration files).
Reviewer: Fable (Opus 4.8). Static review only — no build/test executed.

## Summary

The `types` crate is the foundational data layer for CratonVM: `Value` /
`ObjectRef` (the 16-byte interpreter value enum), `CompactValue` (an 8-byte
NaN-boxed operand-stack slot), `ObjectHeader` + heap layout constants, the
`VmError` / `MethodCallFailed` exception hierarchy, the `ClassId` /
`ClassLoaderId` identity types, the `ACC_*` access flags, and a concurrent
string-interning pool.

This is the **highest-quality crate** I would expect to find in the workspace.
It is essentially pure data + bit-twiddling with no I/O, no classfile parsing,
and no untrusted-length array indexing of its own. There are **no stubs, no
`unimplemented!`/`todo!`, no `NotImplemented`-returning natives, no no-op fake
behavior** — the project's "no synthetic stubs" policy is fully satisfied here
(this crate defines `RuntimeError::NotImplemented` as a *data variant* but never
fakes app behavior). Every non-test `unwrap`/`expect`/`panic!` is absent; the
only `unreachable!`s are provably correct (`& 0x7` on a 3-bit field).

The dominant design risk is **NaN-box long↔object aliasing** in
`compact_value.rs`: a primitive `long` whose 64 bits happen to match the
`SUB_OBJECT` tag pattern is bit-indistinguishable from a real reference. This is
not a latent oversight — it is exhaustively documented, instrumented with a
process-wide degradation counter, and given safe checked decoders
(`to_value_checked`, `is_object_checked`, `decode_by_descriptor`). The unchecked
`to_value()` / `is_object()` paths remain a real soundness hazard *if a caller
ignores the contract*, and the runtime mitigation (GC root scanners filtering
through `VmHeap::is_object_address`) lives outside this crate.

Test coverage is excellent (~92%): 104 inline tests in `compact_value.rs` alone
(including exhaustive 8-sub-tag collision sweeps), full Display/From coverage of
every error variant, layout/offset compile-time asserts plus runtime transmute
pins, a concurrent intern stress test, and two proptest round-trip properties.

### Highest-value findings
- **B1 (low/medium):** `CompactValue::object()` — and therefore the hot
  `from_value(Value::Object(Some(_)))` push path — `assert!`-panics in *release*
  if a heap pointer has any bit ≥47 set (x86-64 5-level paging / AArch64 LVA).
  Documented and guarded by `compile_error!` to x86_64/aarch64, but the panic is
  release-active on the operand-stack hot path with no checked fallback wired in.
- **V1 (medium):** NaN-box long↔object fabrication in unchecked `to_value()` /
  `is_object()` — documented, counted, mitigated by checked siblings + external
  GC validation, but still reachable from attacker-controlled `long` bits if a
  consumer skips the heap check.

## Bugs

### B1 (low/medium) — `from_value` object push panics in release above 47-bit VA
`compact_value.rs:421-435` (`object`), used by `from_value` at
`compact_value.rs:744`.

`CompactValue::object(ptr)` runs `assert!(ptr & !PAYLOAD_MASK == 0, ...)` in
**both** debug and release. `from_value(Value::Object(Some(r)))` calls it with
`r.as_ptr() as u64` on the hot operand-stack/local push path
(`vm/src/runtime/value_stack.rs`, `frame.rs`, `interpreter.rs` all call
`from_value`). On a platform where a legitimately allocated heap pointer has bit
47+ set (x86-64 5-level paging → 57-bit VA; AArch64 LVA → 52-bit VA;
`mmap(MAP_FIXED)` high), pushing a valid reference panics the whole VM.

This is an accepted/documented assumption (the `compile_error!` at
`compact_value.rs:91-98` restricts builds to x86_64/aarch64 where the canonical
lower-half normally holds), and a checked constructor (`try_from_pointer`)
exists. But `from_value` does **not** route through it, so the assumption is
enforced by a release panic rather than graceful degradation. Severity is low in
practice (current targets keep heap in the low 47 bits) but the failure mode is a
hard crash on a path that handles every reference push. Recommendation: have
`from_value` (or a `from_value_checked`) fall back to a defined behavior, or at
minimum make the heap allocator's 47-bit guarantee an explicit, asserted
allocation-time invariant cross-referenced here.

### B2 (low) — `decode_value` `VTAG_LONG`/`VTAG_OBJECT` cannot recover a long-as-jobject smuggled via `VTAG_LONG`
`value.rs:330-353` + `value.rs:372-379`.

`jlong_bits_as_aligned_object_ptr` exists precisely because JNI/internal bridges
surface `jobject` as raw `i64` in a `VTAG_LONG` cell, and those bits must still
be traced as a root. But `decode_value` for `VTAG_LONG` unconditionally returns
`Value::Long` — it never consults `jlong_bits_as_aligned_object_ptr`. This is
correct for the value-decode contract (the helper is for GC scanning, a
different consumer), so it is not a defect in isolation; flagging it as a
consistency note because the two sibling helpers encode subtly different
"is this long actually a pointer" policies and a future caller could conflate
them. No fix required if the GC-side caller is the only user of the helper
(verify that contract holds).

## Vulnerabilities

### V1 (medium) — NaN-box long↔object fabrication in unchecked decoders
`compact_value.rs:812-874` (`to_value`), `:908-911` (`is_object`).

Because `CompactValue::long` stores i64 bits verbatim (intentional, per the BC
SM2 fix), a primitive long whose top 14 bits equal `NANBOX_BITS`, whose 3-bit
sub-tag equals `SUB_OBJECT`, and whose 47-bit payload is non-null + 8-byte
aligned, is **bit-identical to a real heap reference**. `to_value()` will return
`Value::Object(Some(ObjectRef::from_raw(payload)))` — fabricating a heap pointer
from attacker-influenced `lxor`/`ladd` results — and `is_object()` returns
`true`. A context-free consumer that dereferences or GC-roots that result is
acting on a forged reference.

Mitigations present (this is why severity is medium, not high/critical):
- The encoder no longer re-tags (which previously corrupted long values), so the
  hazard is confined to *classification*, not value corruption.
- Checked siblings `to_value_checked` / `is_object_checked`
  (`compact_value.rs:929,966`) fold a live-heap predicate in and degrade a
  fabricated pointer to `Value::Long`, bumping `object_degradation_count`.
- `decode_by_descriptor` (`:1096`) is the type-safe path and never fabricates a
  reference from a primitive.
- Per MEMORY/design docs, the VM's GC root scanners filter every `is_object()`
  slot through `VmHeap::is_object_address`, and `forward_object`'s
  `MAX_SANE_OBJECT_SIZE` guard degrades a stray over-root gracefully (no SEGV).

Residual risk: the safety relies entirely on **every** context-free reference
consumer honoring the contract. The crate cannot enforce this — it is an
external invariant. For open-sourcing, this is the single item most worth a
prominent module-level "SECURITY" note (the contract docs are already excellent;
consider promoting them) and, ideally, a debug-build `#[track_caller]` tripwire
on the unchecked path when `cfg(debug_assertions)`.

### V2 (low) — `intern()` `&'static str` transmute relies on never-drop global
`intern.rs:183-192`.

`intern` `transmute`s an `Arc<str>` byte borrow to `&'static str`. Sound only
because the global pool is a never-dropped `OnceLock` singleton that never calls
`remove`. The code documents this thoroughly, including a WARNING against calling
from `Drop` impls during static teardown. Not a defect; listed because the
`unsafe` transmute is the crate's one lifetime-laundering operation and deserves
visibility in a security review. No action needed beyond keeping the documented
constraint.

## Stubs and Unimplemented

**None.** This is a pure data crate. Grep for
`unimplemented!|todo!|NotImplemented` finds only `RuntimeError::NotImplemented`
(`error.rs:304-306`), which is a legitimate *error-data variant* (the VM throws
it when a feature is genuinely absent elsewhere), not a behavior-faking stub.
`cold_degraded_object_ptr` (`compact_value.rs:327`) is a `#[allow(dead_code)]`
no-op kept only to avoid orphaning a legacy unit-test name — harmless, but a
candidate for deletion (see Feature F5).

## Performance

The crate is already aggressively tuned (`#[inline(always)]` on hot codecs,
`#[cold]`/`#[inline(never)]` on degrade paths, FxHash + RwLock read-fast-path in
the intern pool, single-allocation `Arc<str>` interning). Items below are minor.

### P1 (low) — `format_optional_message` allocates a `String` on every NPE Display
`error.rs:308-313`. `RuntimeError::NullPointerException`'s `Display` calls
`format_optional_message`, which does `format!(": {msg}")` — a heap allocation —
each time the error is formatted. NPE is the single most common JVM exception;
if any path formats these in a loop (logging, repeated `to_string`), this
allocates per call. Low impact (Display is not usually hot) but trivially
avoidable with a `write!`-based helper that takes `&mut Formatter` instead of
returning `String`.

### P2 (low) — `StringPool::intern_arc` allocates the `Arc<str>` before the write-lock re-check
`intern.rs:109-117`. On the slow (miss) path the `Arc::from(s)` allocation
happens before taking the write lock; if a concurrent writer inserted the same
key in the read→write gap, that allocation is discarded. This is an explicit,
documented trade-off (one allocation vs. an extra hash under lock) and is the
right call for a read-heavy workload — noting it only for completeness. No change
recommended.

## Tests

Estimated coverage: **~92%**. Plausibly **reaches 85%** with margin.

Basis (by file):
- `compact_value.rs` — **~95%.** 104 inline tests. Exhaustive collision sweeps
  across all 8 sub-tags for longs (`long_*_collisions_round_trip_exact`),
  dedicated tests for `try_from_pointer` (null/47-bit/max), `update_object_ptr`
  (rewrite/noop/out-of-range/round-trip), `to_value_checked` (fabricated→Long,
  live→Object, non-object passthrough), `is_object_checked`, `decode_by_descriptor`
  (J/D/F/I/byte-short-char-bool/L/null/uninit/unknown/large-magnitude), Debug,
  Eq, and arithmetic round-trips. This is reference-grade.
- `value.rs` — **~90%.** Encode/decode for every variant, degradation-counter
  assertions, the `field_cell_layout` transmute pin. Gap: `ObjectRef::from_raw`
  alignment `debug_assert` is not negatively tested (no `#[should_panic]` on a
  misaligned pointer); `Send`/`Sync` soundness is documented, not testable.
- `heap_types.rs` — **~95%.** Element sizes, `array_data_size` overflow + padding,
  all mark-word transitions incl. CAS, every offset/layout compile-time + runtime
  assert, a `#[should_panic]` for misaligned monitor pointer.
- `error.rs` — **~98%.** Every `Display` string and every `From` conversion is
  asserted, including the IllegalCaller-vs-IllegalState regression guard and the
  no-double-prefix fix.
- `intern.rs` — **~95%.** Pointer-identity, concurrency (scoped threads), empty /
  1 MB / unicode / NUL-byte keys, singleton, both modes share bytes; plus a
  16-thread integration stress test (`intern_stress.rs`).
- `access_flags.rs` / `class_id.rs` / `lib.rs` — **~90%.** Overlapping-flag
  agreement, i32 copies, Display/Hash/Copy, all re-exports.

Most important missing tests:
1. **`from_value(Value::Object(Some(_)))` at the 47-bit boundary (B1).** No test
   exercises a pointer with bit 47 set through `object()`/`from_value`; the
   proptest deliberately caps at `1<<40` (`value_roundtrip.rs:43`), so the
   release-active panic boundary is unverified. Add a `#[should_panic]`
   (object on a >47-bit ptr) and confirm `try_from_pointer` returns `None` there
   (the latter exists; the panic boundary does not).
2. **`ObjectRef::from_raw` misaligned-pointer `debug_assert`** — add a
   `#[should_panic]` in a debug build to pin the contract.
3. **`update_object_ptr_unchecked` debug assertion** on an out-of-range pointer
   (`compact_value.rs:1048`) — currently only the checked variant's rejection is
   tested.
4. **Fuzz/property test that `to_value()` never dereferences** — feed random
   u64s as `CompactValue::from_bits` and assert `to_value()` does not panic and
   that any `Object(Some)` it returns has an aligned non-null payload (codifies
   the V1 contract boundary).

## Feature Suggestions

- **F1 — Debug-build tripwire on unchecked `to_value`/`is_object` reference
  results.** Under `cfg(debug_assertions)`, count or `eprintln!`-once when an
  unchecked decode yields `Object(Some)` from a slot that could be a long, so
  fuzzers surface contract violations even when the external heap check is the
  intended guard. Complements the existing `object_degradation_count`.
- **F2 — `from_value_checked` / make `from_value` non-panicking (addresses B1).**
  Provide a fallible object-push that returns `Option`/`Result` instead of
  `assert!`-panicking, and route the interpreter's reference push through it (or
  document the heap-allocator 47-bit guarantee as an asserted invariant at the
  allocation site).
- **F3 — Hoist the long↔object hazard into a crate-level `//! # SECURITY`
  section.** The per-method docs are superb; a single discoverable summary at the
  top of `compact_value.rs` would serve open-source readers and security auditors.
- **F4 — `StringPool::shrink_to_fit` / capacity metrics.** The pool never frees;
  exposing `capacity()` / a memory estimate would help long-running daemon apps
  (Tomcat/WildFly) reason about interned-string footprint.
- **F5 — Delete the dead `cold_degraded_object_ptr` no-op** (`compact_value.rs:324-329`)
  once the legacy test that names it is updated, removing the `#[allow(dead_code)]`.
- **F6 — Const-generic / typed `ClassId` newtypes** (e.g. a distinct
  `ArrayClassId`) to catch ID-category mix-ups at compile time; low priority,
  ergonomic only.

## Files sampled vs fully read

Fully read (100%):
- `types/src/lib.rs`
- `types/src/access_flags.rs`
- `types/src/class_id.rs`
- `types/src/value.rs`
- `types/src/heap_types.rs`
- `types/src/error.rs`
- `types/src/intern.rs`
- `types/tests/value_roundtrip.rs`
- `types/tests/intern_stress.rs`

Read in full for non-test code + sampled tests (`compact_value.rs`, 2581 lines):
- Lines 1-360 (constants, NaN-box scheme, degradation counter) — read fully.
- Lines 360-1209 (entire `impl CompactValue`, `From`, `Debug`/`Eq`) — read fully;
  this is all the production logic.
- Test module (1261-2581) — structure grepped (104 `#[test]`s enumerated) and
  the collision-sweep / checked-decoder / descriptor regions (1440-1570) deep-read
  to confirm coverage breadth; remaining arithmetic/round-trip tests sampled by
  name via grep rather than line-by-line.
