# Fixed suite bugs (archive)

**Non-normative history.** These are the original per-suite bug/crash reports for
defects that are now **FIXED on `dev`**, moved here out of the active trackers so
that `docs/known-issues/` holds only **open** items. Kept for traceability (repro
+ root cause + fix commit); do not cite as current behaviour.

| Doc | Fix |
|---|---|
| `bug-03-priorityqueue-boxed-ordering.md` | `936b8e19` — verified vs HotSpot |
| `crash-01-arraylist-capacity-oom-abend.md` | `fix/oom-array-alloc-abend` (base `0e3f0398`) |
| `crash-02-native-capacity-ctor-abort-family.md` | `da58ff4e` — verified vs HotSpot |
| `crash-03-rsocket-payloadutils-jit-sigsegv.md` | `957270c8` — verified |
| `spring-bug-02-kotlin-reflect-builtins-null.md` | `cf58ea48` (SB-15) — symptom resolved |
| `spring-bug-03-stream-iface-no-code-attribute.md` | `4c5a4d09` (merge `d75f7716`) — verified |
| `spring-bug-04-junit-timeout-interceptor-double-proceed.md` | `fc20e970` — `compare_via_compare_to` throws CCE for non-`Comparable`; `ValueCodeGeneratorTests` 41/41 vs HotSpot. "Interceptor invoked twice" was a mask, not threading/MH. |
| `spring-bug-05-dynamic-proxy-module-system.md` | `16f832c0` (merge `d75f7716`) |
| `spring-bug-12-hashmap-view-spliterator-int16.md` | `fix/spring-bug-10-11` — validated |
| `bug-A-arraylist-sublist-copy-not-view.md` | `9a535e85` (off dev `b8908…`) |
| `springsuite-0620-toarray-referencepipeline-recursion.md` | `8795b88d` — `ReferencePipeline.toArray(IntFunction)` native re-entered no-arg `toArray()` → `StackOverflowError`; was the `MergedAnnotations` hang + bug-06 fam6 "~2 GB OOM" and blocked the whole JUnit suite. `MergedAnnotationsTests` 174/178, `AnnotationUtilsTests` 72/72. |
| `springsuite-0619-unsafe-offheap-directbuffer.md` | `3b16e985` (bug-A) + re-verified 2026-06-20 (bug-A2 also resolved) — `PooledDataBufferTests` 10/10, `LeakAwareDataBufferFactoryTests` 2/2. |
| `SC-map-multivaluemap-family.md` | RC-1 keySet `contains`→`containsKey` + RC-2 LHM `putIfAbsent` null-replace (earlier on dev) + RC-3 `Map.equals` foreign-Map operand via virtual dispatch (`e115b0bd`). All 3 native-collections Map bugs verified vs HotSpot (JDK 25, `test_classes/MapEqRepro` 15/15). RC-4/RC-5 are non-Map ByteBuddy/Mockito handoffs (bug-E). |
| `SC-aot-runtimehints-resource-count.md` | `Stream.distinct()` (`native_stream_distinct`) deduped via shallow `values_equal` → value classes/records never collapsed → over-counted resource globs (8 vs 5). Now dedups via `list_element_matches` (real Java `equals`), `029f2c87` (merge `cf269fc5`). Verified vs HotSpot (JDK 25, `test_classes/DistinctEquals` 5/5). 4 non-`distinct()` writer tests = unconfirmed separate residual (re-triage if they fail). |
| `restclientextensions-mockk-verify-ptr-arg.md` | Current `dev` (`89db75ae`) no longer reproduces the mockk `ParameterizedTypeReference` verify mismatch; exact causal commit not isolated. Verified 2026-07-01: `RestClientExtensionsTests` 5/5 in real-JDK, JIT-on mode, and also 5/5 with `CRATONVM_XT_JIT_ROOT_SCAN=0`. |
| `bug06-fam5-reflection-getdeclaredmethod-null.md` | ✅ CLOSED 2026-07-02 — the `getDeclaredMethod on null` ×28 aggregate is **extinct**: 0 instances in the clean full re-runs (dev `d707c97e` 2026-06-30, `f7506e02` 2026-07-01) and in a fresh 196-class nojit+jit sweep of the reflection/annotation surface on dev `ffb247e5`; `Refl5` probe ==HotSpot both modes. Was a cross-family cascade (fam1/3/4 + `toArray` recursion + bug-04 GC + bug-05 generics), all sources fixed. Repro kept at `repros/bug06-fam5-reflection-null/Refl5.java`. |
| `keycloak-previewfeatures-ispreviewenabled-native.md` | Added `jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z` native (`native-builtins/src/lib.rs`, `register_essential_natives`), returning `false` to match HotSpot's no-`--enable-preview` default. Collapsed all 1044/1044 crashes in the Keycloak Azure non-passed rerun (same shared JUnit launcher/discovery path). Verified locally on JDK 25: pre-fix binary reproduces the reported `UnsatisfiedLinkError`, post-fix binary returns `false` matching HotSpot. `--enable-preview` itself is still unparsed (roadmap item) — only the default (off) path is covered. Full Azure rerun not yet re-executed. |
| `hibernate-bytearraymapping-stackwalk-gc-corruption.md` | `codex/hib-bytearray-stackwalk-gc-20260703` — native constructor helpers pin object constructor args across allocation/class-init GC; `ByteArrayMappingTests` 2/2 no longer SIGSEGVs. |

**Not moved — partial fixes with open residuals** (left in their suite folders / `docs/known-issues/`):
- `spring-suite/crash-reports-2026-06-16/bug-05-generics-fieldtypesignature-cce.md`
  — wildcard-bound `Type[]` fixed (`d01345d1`); real-`ParameterizedType` residual open.
- `spring-suite/bugs/spring-bug-09-collection-layout-probe-oob.md`
  — crash fixed (`5941addd`); a separate residual hang is still open.
- `docs/known-issues/springsuite-0619-getbeanclassname-bean-filter.md`
  — bug-B bean-filter fixed (`3b16e985`); bug-B2 CGLIB **method-injection** null-instance still
  open (`LookupMethodTests` 0/7, "Target object must not be null").
