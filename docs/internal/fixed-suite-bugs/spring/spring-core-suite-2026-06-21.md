# CratonVM × Spring Framework suite — bug index

> **ARCHIVED 2026-07-01:** This is the historical index for the 2026-06-21
> `spring-core` sweep. It no longer owns active bug state in `docs/known-issues`;
> fixed/refuted items are marked inline and residual handoffs are tracked by their
> focused docs in this `docs/internal/spring` folder or broader cross-cutting
> workstreams.

Worktree: `CratonVM-spring0621` (binary built from dev `0c904c04`). Each Spring test
**passes on HotSpot/JDK25**, so every failure under CratonVM is a divergence (VM bug)
unless tagged **[harness/env]**. Reports are root-cause clusters, not per-test.

Legend: **FIX** = contained, I can do it · **HANDOFF** = deep/risky/cross-cutting.

## Fixed
Branch `fix/spring-core-suite-bugs` (off latest dev `6eeb1fe6`); each verified vs the real test class.
| ID | Sev | Status | Result |
|----|-----|--------|--------|
| W1 | low-med | ✅ FIXED `7f99971b` (own branch; merged to dev) | Stream slot-1 close-handler OOB |
| misc-core chm/lhm | med | ✅ FIXED `482b3d96` | `CHM.remove(null)` NPE + `LHM.putIfAbsent` null-replace → SimpleAliasRegistry 12/12 |
| generic-type-signature-cce | high | ✅ FIXED `f0d2d68f` | reify bounds → Type[] → ResolvableType 155→161/162 (6 CCEs gone) |
| map-multivaluemap | med | ✅ FIXED `63c120af` | keySet().contains honors overridden containsKey → LinkedCaseInsensitiveMap 18/18 |
| [stax-xml-family](SC-stax-xml-family.md) | med | ◑ PARTIAL `cd8c9b9f` | 4 cursor natives + getName prefix → StaxStream 1/6→4/6 (2 namespace-SAX-sequence tests remain) |
| annotation-introspection **Bug B** | high | ✅ FIXED `00c71bb6` | lambda SAM dispatch now checks param types → AnnotationFilter 11/11, AnnotationTypeMappings 43/43, MergedAnnotationsRepeatable 24/24 (general correctness fix) |
| [jspecify-nullness-reflection](SC-jspecify-nullness-reflection.md) | med | FIXED 2026-07-01 | TYPE_USE `AnnotatedType` and package-info annotations are implemented; native-level regression coverage added for return/parameter/field type-use annotations |
| [stream-collector-supplier-no-code](../SC-stream-collector-supplier-no-code.md) | med | FIXED 2026-07-01 | synthetic `Collector` supplier/accumulator/finisher/combiner plus returned functional-interface SAMs are registered; registry regression coverage added |

**Session total: 9 bugs fixed, ~44 spring-core tests recovered. Branch rebased onto current dev (merge `75ffb42c`).**
Remaining annotation-introspection sub-bugs (same report): A (TypeNotPresent/classloader, AnnotationIntrospectionFailureTests 0/4), C (enclosing-class scan), D (bridge-method) — separate deeper fixes.

## spring-core — open clusters (run 1, 262 classes)
| ID | Sev | Conf | Rec | Tests | Root cause |
|----|-----|------|-----|-------|-----------|
| generic-type-signature-cce | high | high | ✅FIXED | 9 | stale duplicate of the fixed row above; real `TypeVariableImpl.getBounds()` reifier not overridden → CCE `FieldTypeSignature→Type[]` |
| [annotation-introspection-family](SC-annotation-introspection-family.md) | high | high | FIX | 12 | 4 distinct: Class-attr TypeNotPresent, lambda SAM/default overload, classloader ctx, bridge merge |
| [env-classreading](SC-env-classreading.md) | high | high | FIX+HO | 6 | ~~getenv/getProperties identity~~ **✅fixed on dev** (`fea93ba8`); `Object.equals` shadows override (precedenceOf=-1) [open,HO]; ~~`int.class`→Integer~~ **✅fixed on dev**; custom-ClassLoader `getResourceAsStream`=null [open,HO] — triaged 2026-06-22, see doc |
| [stax-xml-family](SC-stax-xml-family.md) | med | high | FIX | 7 | StAX→SAX bridge: 3 unregistered cursor natives + `getName()` drops element prefix |
| [map-multivaluemap-family](../SC-map-multivaluemap-family.md) | med | high | ✅FIXED | 6 | keySet.contains skips `containsKey` override; LHM putIfAbsent drops null-replace; Map.equals fails on foreign-Map arg — **all 3 fixed on dev** (RC-3 `e115b0bd`); archived. +2 ByteBuddy handoff (bug-E) |
| [aot-runtimehints-resource-count](../SC-aot-runtimehints-resource-count.md) | med | high | ✅FIXED | 3 | `Stream.distinct()` ignored Java equals/hashCode → dup resource globs (8 vs 5) — **fixed on dev** (`029f2c87`); archived. (4 reflection/jni/etc writer tests = unconfirmed separate residual.) |
| [task-retry-util-misc](SC-task-retry-util-misc.md) | med | med-high | MIXED | 14 | ~~non-Serializable unmod-map~~ fixed; ~~Properties.store missing #date~~ fixed; Throwable deser; retry 20ms timing; **ByteBuddy ClassInjector [handoff]**; AQS throttle |
| [resource-io-family](SC-resource-io-family.md) | med | high | FIX | ~25 | ~~NIO write-channel stub no `write`~~ **✅A fixed** (`fd14c4ea`); `Path.toUri()` `file://` = **⚪not-a-bug** (HotSpot matches on Win); ~~`newOutputStream`-on-dir exception type~~ **✅C fixed** (`9e14cc2d`, AccessDeniedException); **many FileNotFoundException are harness CWD [env]**; E/F/G handoffs — triaged 2026-06-22, see doc |
| [misc-core-spring](SC-misc-core-spring.md) | med | med | MIXED | 3 | ~~CHM null-key NPE parity~~ fixed; SortedProperties OutputStream store remains handoff |

## spring-core — HANGS (timeouts)
| ID | Sev | Rec | Class | Hypothesis |
|----|-----|-----|-------|-----------|
| [hangs-mergedannotations-charsequence](SC-hangs-mergedannotations-charsequence.md) | high | investigate | MergedAnnotationsTests | real-`ReferencePipeline` `stream().toArray()` re-enters native toArray→stream_elements→real toArray (fam6 bounce) |
| ″ | high | handoff | codec.CharSequenceEncoderTests | Reactor `StepVerifier.verify()` blocks on latch; `ExecutorService.execute()` runs inline + scheduled-task pump not driven → producer never runs |

## Cross-cutting (affect many modules — prioritize)
- **ByteBuddy `ClassInjector$UsingReflection` "Could not create type"** → breaks AssertJ `assertSoftly` (SoftAssertions) and Mockito. Will recur widely (spring-test/webmvc). **[handoff]**
- **JUnit "called invocation multiple times: TimeoutExtension"** masks an underlying VM error (see memory `junit-multiple-times-masks-vm-linkage-error`).
- **CWD-relative resource tests** → `FileNotFoundException` from harness working dir, not a VM bug. Re-check tally; consider running per-module from the module dir.

## Notes
- NOTES-prefiltered-warns.md — W1 stream-layout WARN (now fixed) + filter rationale.
